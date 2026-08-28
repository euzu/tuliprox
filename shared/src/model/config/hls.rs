use crate::{
    defaults::{
        default_hls_cache_bytes, default_hls_cache_bytes_per_session, default_hls_cache_duration,
        default_hls_corrupt_segment_watchdog_max_parallel_jobs, default_hls_initial_manifest_wait_timeout_secs,
        default_hls_max_concurrent_segment_fetches_global, default_hls_max_concurrent_segment_fetches_per_session,
        default_hls_max_segments_prefetch, default_hls_origin_manifest_timeout_ms,
        default_hls_origin_segment_timeout_ms, default_hls_segment_repair_apply_to_first_segments,
        default_hls_segment_repair_high_size_increase_percent, default_hls_segment_repair_low_size_increase_percent,
        default_hls_segment_repair_max_parallel_repairs, default_hls_segment_repair_medium_size_increase_percent,
        default_hls_segment_repair_postprocess_timeout_ms, default_hls_session_idle_timeout, DEFAULT_HLS_CACHE_BYTES,
        DEFAULT_HLS_CACHE_BYTES_PER_SESSION,
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
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub const fn clean(&mut self) {}
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsManifestRecoveryBurstConfigDto {
    #[serde(default)]
    pub level: HlsManifestRecoveryBurstLevel,
}

impl HlsManifestRecoveryBurstConfigDto {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub const fn clean(&mut self) {}
}

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
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub const fn clean(&mut self) {}

    fn validate(&self) -> Result<(), TuliproxError> {
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
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub fn clean(&mut self) {
        self.size_increase.clean();
        self.corrupt_segment_watchdog.clean();
    }

    fn validate(&self) -> Result<(), TuliproxError> {
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
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub const fn clean(&mut self) {}

    fn validate(&self) -> Result<(), TuliproxError> {
        if self.max_parallel_jobs == 0 {
            return Err(TuliproxError::ConfigReverseProxy(
                "hls_cache.segment_repair.corrupt_segment_watchdog.max_parallel_jobs must be >= 1".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsCacheConfigDto {
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
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

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
mod tests {
    use super::{
        ByteSize, HlsCacheConfigDto, HlsCorruptSegmentWatchdogConfigDto, HlsCorruptSegmentWatchdogMode,
        HlsManifestRecoveryBurstConfigDto, HlsManifestRecoveryBurstLevel, HlsSegmentRepairConfigDto,
        HlsSegmentRepairMode, HlsSegmentRepairSizeIncreaseConfigDto, HlsStripConfigDto, HlsStripMode, Millis, Secs,
    };
    use crate::model::ReverseProxyConfigDto;

    #[test]
    fn hls_cache_deserializes_target_yaml() {
        let yaml = r#"
rewrite_secret: 00112233445566778899aabbccddeeff
hls_cache:
  cache_path:
  strip:
    mode: "segments"
    value: 0
  cache_duration: 300
  cache_bytes: "10GB"
  cache_bytes_per_session: "512MB"
  max_segments_prefetch: 6
  max_concurrent_segment_fetches_per_session: 2
  max_concurrent_segment_fetches_global: 64
  origin_manifest_timeout_ms: 3000
  origin_segment_timeout_ms: 10000
  session_idle_timeout: 300
"#;

        let cfg: ReverseProxyConfigDto = serde_saphyr::from_str(yaml).expect("reverse_proxy should deserialize");
        assert_eq!(cfg.hls_cache, Some(HlsCacheConfigDto::default()));
    }

    #[test]
    fn hls_cache_serializes_under_reverse_proxy() {
        let cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto::default()),
            ..Default::default()
        };

        let serialized = serde_saphyr::to_string(&cfg).expect("serialization should succeed");
        assert!(serialized.contains("hls_cache:"), "expected hls_cache block, got: {serialized}");
    }

    #[test]
    fn hls_cache_serializes_non_default_segment_repair() {
        let cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto {
                segment_repair: HlsSegmentRepairConfigDto {
                    max_level: HlsSegmentRepairMode::Medium,
                    apply_to_first_segments: 2,
                    max_parallel_repairs: 2,
                    postprocess_timeout_ms: Millis::new(1_500),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        let serialized = serde_saphyr::to_string(&cfg).expect("serialization should succeed");
        assert!(serialized.contains("segment_repair:"), "expected segment_repair block, got: {serialized}");
        assert!(serialized.contains("max_level: medium"), "expected segment repair max level, got: {serialized}");
        assert!(
            !serialized.contains("mode: medium"),
            "segment repair must serialize the v2 max_level field, got: {serialized}"
        );
        assert!(
            serialized.contains("apply_to_first_segments: 2"),
            "expected segment repair first segment limit, got: {serialized}"
        );
        assert!(
            serialized.contains("max_parallel_repairs: 2"),
            "expected segment repair parallel limit, got: {serialized}"
        );
        assert!(
            serialized.contains("postprocess_timeout_ms: 1500"),
            "expected common post-processing timeout, got: {serialized}"
        );
    }

    #[test]
    fn hls_cache_serializes_non_default_corrupt_segment_watchdog_without_command_version() {
        let cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto {
                segment_repair: HlsSegmentRepairConfigDto {
                    corrupt_segment_watchdog: HlsCorruptSegmentWatchdogConfigDto {
                        mode: HlsCorruptSegmentWatchdogMode::Sanitize,
                        max_parallel_jobs: 2,
                    },
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        let serialized = serde_saphyr::to_string(&cfg).expect("serialization should succeed");
        assert!(serialized.contains("corrupt_segment_watchdog:"), "expected watchdog config block, got: {serialized}");
        assert!(serialized.contains("mode: sanitize"), "expected watchdog mode, got: {serialized}");
        assert!(serialized.contains("max_parallel_jobs: 2"), "expected watchdog parallel limit, got: {serialized}");
        assert!(
            !serialized.contains("command_version"),
            "command_version is internal and must not serialize, got: {serialized}"
        );
    }

    #[test]
    fn hls_cache_serializes_non_default_manifest_recovery_burst() {
        let cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto {
                manifest_recovery_burst: HlsManifestRecoveryBurstConfigDto {
                    level: HlsManifestRecoveryBurstLevel::Beast,
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        let serialized = serde_saphyr::to_string(&cfg).expect("serialization should succeed");
        assert!(
            serialized.contains("manifest_recovery_burst:"),
            "expected manifest recovery burst block, got: {serialized}"
        );
        assert!(serialized.contains("level: beast"), "expected burst level, got: {serialized}");
    }

    #[test]
    fn hls_cache_rejects_corrupt_segment_watchdog_command_version() {
        let yaml = r"
rewrite_secret: 00112233445566778899aabbccddeeff
hls_cache:
  segment_repair:
    corrupt_segment_watchdog:
      mode: sanitize
      command_version: 1
";

        let err = serde_saphyr::from_str::<ReverseProxyConfigDto>(yaml)
            .expect_err("command_version must be rejected as unknown config");
        assert!(err.to_string().contains("command_version"), "unexpected error: {err}");
    }

    #[test]
    fn hls_cache_serializes_non_default_segment_repair_size_increase() {
        let segment_repair = HlsSegmentRepairConfigDto {
            max_level: HlsSegmentRepairMode::Medium,
            size_increase: HlsSegmentRepairSizeIncreaseConfigDto { medium_percent: 9, ..Default::default() },
            ..Default::default()
        };
        let cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto { segment_repair, ..Default::default() }),
            ..Default::default()
        };

        let serialized = serde_saphyr::to_string(&cfg).expect("serialization should succeed");
        assert!(serialized.contains("size_increase:"), "expected size increase block, got: {serialized}");
        assert!(serialized.contains("medium_percent: 9"), "expected size increase value, got: {serialized}");
    }

    #[test]
    fn hls_cache_with_non_default_segment_repair_is_not_empty() {
        let cfg = HlsCacheConfigDto {
            segment_repair: HlsSegmentRepairConfigDto {
                max_level: HlsSegmentRepairMode::Medium,
                apply_to_first_segments: 1,
                max_parallel_repairs: 1,
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(!cfg.is_empty());
    }

    #[test]
    fn hls_cache_rejects_unknown_fields() {
        let yaml = r#"
rewrite_secret: 00112233445566778899aabbccddeeff
hls_cache:
  cache_path: "/tmp/tuliprox/cache/hls"
  unknown_field: true
"#;

        let err = serde_saphyr::from_str::<ReverseProxyConfigDto>(yaml).expect_err("unknown fields must be rejected");
        assert!(err.to_string().contains("unknown_field"), "unexpected error: {err}");
    }

    #[test]
    fn hls_cache_prepare_sets_defaults_for_blank_values() {
        let mut cfg = HlsCacheConfigDto {
            cache_path: None,
            cache_bytes: ByteSize::new(" "),
            cache_bytes_per_session: ByteSize::new(""),
            ..Default::default()
        };

        cfg.prepare().expect("prepare should succeed");

        assert_eq!(cfg, HlsCacheConfigDto::default());
    }

    #[test]
    fn hls_cache_prepare_accepts_default_values() {
        let mut cfg = HlsCacheConfigDto::default();

        cfg.prepare().expect("default hls cache config should be valid");
    }

    #[test]
    fn hls_cache_prepare_allows_zero_prefetch() {
        let mut cfg = HlsCacheConfigDto { max_segments_prefetch: 0, ..Default::default() };

        cfg.prepare().expect("zero prefetch should be valid");
    }

    #[test]
    fn hls_cache_prepare_rejects_invalid_minimums() {
        let cases: [(&str, HlsCacheConfigDto); 6] = [
            ("cache_duration", HlsCacheConfigDto { cache_duration: Secs::new(0), ..Default::default() }),
            (
                "max_concurrent_segment_fetches_per_session",
                HlsCacheConfigDto { max_concurrent_segment_fetches_per_session: 0, ..Default::default() },
            ),
            (
                "max_concurrent_segment_fetches_global",
                HlsCacheConfigDto { max_concurrent_segment_fetches_global: 0, ..Default::default() },
            ),
            (
                "origin_manifest_timeout_ms",
                HlsCacheConfigDto { origin_manifest_timeout_ms: Millis::new(0), ..Default::default() },
            ),
            (
                "origin_segment_timeout_ms",
                HlsCacheConfigDto { origin_segment_timeout_ms: Millis::new(0), ..Default::default() },
            ),
            ("session_idle_timeout", HlsCacheConfigDto { session_idle_timeout: Secs::new(0), ..Default::default() }),
        ];

        for (field_name, mut cfg) in cases {
            let err = cfg.prepare().expect_err("invalid minimum should be rejected");
            assert!(err.to_string().contains(field_name), "expected error to mention {field_name}, got: {err}");
        }
    }

    #[test]
    fn hls_cache_prepare_rejects_invalid_segment_repair_limits() {
        let mut too_many_first_segments = HlsCacheConfigDto::default();
        too_many_first_segments.segment_repair.apply_to_first_segments = 7;
        let err = too_many_first_segments.prepare().expect_err("too many first segments should be rejected");
        assert!(err.to_string().contains("segment_repair.apply_to_first_segments"), "unexpected error: {err}");

        let mut too_many_parallel_repairs = HlsCacheConfigDto { max_segments_prefetch: 2, ..Default::default() };
        too_many_parallel_repairs.segment_repair.max_level = HlsSegmentRepairMode::Medium;
        too_many_parallel_repairs.segment_repair.max_parallel_repairs = 3;
        let err = too_many_parallel_repairs.prepare().expect_err("too many parallel repairs should be rejected");
        assert!(err.to_string().contains("segment_repair.max_parallel_repairs"), "unexpected error: {err}");

        let mut invalid_size_increase = HlsCacheConfigDto::default();
        invalid_size_increase.segment_repair.size_increase.high_percent = 101;
        let err = invalid_size_increase.prepare().expect_err("size increase above 100 should be rejected");
        assert!(err.to_string().contains("segment_repair.size_increase.high_percent"), "unexpected error: {err}");

        let mut invalid_postprocess_timeout = HlsCacheConfigDto::default();
        invalid_postprocess_timeout.segment_repair.postprocess_timeout_ms = Millis::new(99);
        let err =
            invalid_postprocess_timeout.prepare().expect_err("post-processing timeout below 100ms should be rejected");
        assert!(err.to_string().contains("segment_repair.postprocess_timeout_ms"), "unexpected error: {err}");

        let mut invalid_watchdog_parallel = HlsCacheConfigDto::default();
        invalid_watchdog_parallel.segment_repair.corrupt_segment_watchdog.max_parallel_jobs = 0;
        let err = invalid_watchdog_parallel.prepare().expect_err("watchdog parallelism below 1 should be rejected");
        assert!(
            err.to_string().contains("segment_repair.corrupt_segment_watchdog.max_parallel_jobs"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hls_cache_prepare_allows_zero_segment_repair_parallelism_when_repair_is_off() {
        let mut cfg = HlsCacheConfigDto::default();
        cfg.segment_repair.max_parallel_repairs = 0;

        cfg.prepare().expect("zero repair parallelism is ignored while repair is off");
    }

    #[test]
    fn hls_cache_prepare_rejects_zero_segment_repair_parallelism_when_repair_is_enabled() {
        let mut cfg = HlsCacheConfigDto::default();
        cfg.segment_repair.max_level = HlsSegmentRepairMode::Medium;
        cfg.segment_repair.max_parallel_repairs = 0;

        let err = cfg.prepare().expect_err("enabled repair needs a positive parallel limit");
        assert!(err.to_string().contains("segment_repair.max_parallel_repairs"), "unexpected error: {err}");
    }

    #[test]
    fn hls_strip_config_default_is_segments() {
        let cfg = HlsStripConfigDto::default();
        assert_eq!(cfg.mode, HlsStripMode::Segments);
        assert_eq!(cfg.value, 0);
    }
}
