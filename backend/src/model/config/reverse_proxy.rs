use crate::model::config::cache::CacheConfig;
use crate::model::{macros, GeoIpConfig, QosAggregationConfig, RateLimitConfig, StreamConfig};
use regex::Regex;
use shared::model::{
    HlsCacheConfigDto, HlsCorruptSegmentWatchdogConfigDto, HlsManifestRecoveryBurstConfigDto,
    HlsSegmentRepairConfigDto, HlsSegmentRepairSizeIncreaseConfigDto, ResourceRetryConfigDto, ReverseProxyConfigDto,
    ReverseProxyDisabledHeaderConfigDto, HlsStripMode, REGEX_CACHE,
};
use shared::defaults::{default_resource_retry_attempts, default_resource_retry_backoff_ms, default_resource_retry_backoff_multiplier, };
use shared::utils::{hex_to_u8_16, u8_16_to_hex};
use std::cmp::max;
use std::sync::Arc;
use shared::defaults::HLS_CACHE_DIR_SUFFIX;

#[derive(Debug, Clone)]
pub struct ReverseProxyDisabledHeaderConfig {
    pub referer_header: bool,
    pub x_header: bool,
    pub cloudflare_header: bool,
    pub custom_header: Vec<String>,
}

impl ReverseProxyDisabledHeaderConfig {
    pub fn should_remove(&self, header: &str) -> bool {
        let header_lc = header.to_ascii_lowercase();
        if self.referer_header && header_lc == "referer" {
            return true;
        }
        if self.x_header && header_lc.starts_with("x-") {
            return true;
        }
        if self.cloudflare_header && header_lc.starts_with("cf-") {
            return true;
        }
        self.custom_header
            .iter()
            .any(|h| h.trim().eq_ignore_ascii_case(&header_lc))
    }
}

#[derive(Debug, Clone)]
pub struct ResourceRetryConfig {
    pub max_attempts: u32,
    pub backoff_millis: u64,
    pub backoff_multiplier: f64,
    pub failover_redirect_patterns: Vec<Arc<Regex>>,
}

impl Default for ResourceRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_resource_retry_attempts(),
            backoff_millis: default_resource_retry_backoff_ms(),
            backoff_multiplier: default_resource_retry_backoff_multiplier(),
            failover_redirect_patterns: default_failover_redirect_patterns(),
        }
    }
}

/// Default failover redirect pattern when none is configured
fn default_failover_redirect_patterns() -> Vec<Arc<Regex>> {
    vec![REGEX_CACHE
        .get_or_compile("service-abuse")
        .unwrap_or_else(|err| unreachable!("hardcoded failover regex 'service-abuse' must compile: {err}"))]
}

impl ResourceRetryConfig {
    pub fn get_retry_values(&self) -> (u32, u64, f64) {
        (
            max(1, self.max_attempts),
            self.backoff_millis.max(1),
            if self.backoff_multiplier.is_finite() {
                self.backoff_multiplier.max(1.0)
            } else {
                1.0
            },
        )
    }

    pub fn get_default_retry_values() -> (u32, u64, f64) {
        (
            default_resource_retry_attempts(),
            default_resource_retry_backoff_ms(),
            default_resource_retry_backoff_multiplier(),
        )
    }
}

macros::from_impl!(ResourceRetryConfig);

impl From<&ResourceRetryConfigDto> for ResourceRetryConfig {
    fn from(dto: &ResourceRetryConfigDto) -> Self {
        let multiplier = if dto.backoff_multiplier.is_finite() {
            dto.backoff_multiplier.max(1.0)
        } else {
            1.0
        };
        
        // Compile patterns, default to service-abuse if none or empty
        let patterns = dto.failover_redirect_patterns
            .as_ref()
            .filter(|v| !v.is_empty())
            .map_or_else(default_failover_redirect_patterns, |patterns| {
                patterns.iter()
                    .filter_map(|p| REGEX_CACHE.get_or_compile(p).map_err(|e| {
                        log::warn!("Failed to compile failover redirect pattern '{p}': {e}");
                        e
                    }).ok())
                    .collect()
            });
        
        Self {
            max_attempts: dto.max_attempts,
            backoff_millis: dto.backoff_millis,
            backoff_multiplier: multiplier,
            failover_redirect_patterns: patterns,
        }
    }
}

impl From<&ResourceRetryConfig> for ResourceRetryConfigDto {
    fn from(cfg: &ResourceRetryConfig) -> Self {
        let patterns: Vec<String> = cfg.failover_redirect_patterns
            .iter()
            .map(|re| re.as_str().to_string())
            .collect();
        Self {
            max_attempts: cfg.max_attempts,
            backoff_millis: cfg.backoff_millis,
            backoff_multiplier: cfg.backoff_multiplier,
            failover_redirect_patterns: if patterns.is_empty() { None } else { Some(patterns) },
        }
    }
}

// Runtime config structs hold the same enum types as their DTO counterparts
// (no mirror enums). `is_enabled`, `as_log_value`, and `plan` live on the
// shared enums (`shared::model::HlsCorruptSegmentWatchdogMode`,
// `shared::model::HlsManifestRecoveryBurstLevel`) so behavior stays in one
// place. The From impls below become trivial field-by-field clones.

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StripConfig {
    pub mode: HlsStripMode,
    pub value: u64,
}

impl From<&shared::model::HlsStripConfigDto> for StripConfig {
    fn from(dto: &shared::model::HlsStripConfigDto) -> Self {
        Self { mode: dto.mode, value: dto.value }
    }
}

impl From<&StripConfig> for shared::model::HlsStripConfigDto {
    fn from(config: &StripConfig) -> Self {
        Self { mode: config.mode, value: config.value }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsManifestRecoveryBurstConfig {
    pub level: shared::model::HlsManifestRecoveryBurstLevel,
}

impl Default for HlsManifestRecoveryBurstConfig {
    fn default() -> Self { Self::from(&HlsManifestRecoveryBurstConfigDto::default()) }
}

impl From<&HlsManifestRecoveryBurstConfigDto> for HlsManifestRecoveryBurstConfig {
    fn from(dto: &HlsManifestRecoveryBurstConfigDto) -> Self {
        Self { level: dto.level }
    }
}

impl From<&HlsManifestRecoveryBurstConfig> for HlsManifestRecoveryBurstConfigDto {
    fn from(config: &HlsManifestRecoveryBurstConfig) -> Self {
        Self { level: config.level }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsSegmentRepairSizeIncreaseConfig {
    pub low_percent: u8,
    pub medium_percent: u8,
    pub high_percent: u8,
}

impl Default for HlsSegmentRepairSizeIncreaseConfig {
    fn default() -> Self { Self::from(&HlsSegmentRepairSizeIncreaseConfigDto::default()) }
}

impl From<&HlsSegmentRepairSizeIncreaseConfigDto> for HlsSegmentRepairSizeIncreaseConfig {
    fn from(dto: &HlsSegmentRepairSizeIncreaseConfigDto) -> Self {
        Self {
            low_percent: dto.low_percent,
            medium_percent: dto.medium_percent,
            high_percent: dto.high_percent,
        }
    }
}

impl From<&HlsSegmentRepairSizeIncreaseConfig> for HlsSegmentRepairSizeIncreaseConfigDto {
    fn from(config: &HlsSegmentRepairSizeIncreaseConfig) -> Self {
        Self {
            low_percent: config.low_percent,
            medium_percent: config.medium_percent,
            high_percent: config.high_percent,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsSegmentRepairConfig {
    pub max_level: shared::model::HlsSegmentRepairMode,
    pub apply_to_first_segments: u8,
    pub max_parallel_repairs: usize,
    pub postprocess_timeout_ms: u64,
    pub size_increase: HlsSegmentRepairSizeIncreaseConfig,
    pub corrupt_segment_watchdog: HlsCorruptSegmentWatchdogConfig,
}

impl Default for HlsSegmentRepairConfig {
    fn default() -> Self { Self::from(&HlsSegmentRepairConfigDto::default()) }
}

impl From<&HlsSegmentRepairConfigDto> for HlsSegmentRepairConfig {
    fn from(dto: &HlsSegmentRepairConfigDto) -> Self {
        Self {
            max_level: dto.max_level,
            apply_to_first_segments: dto.apply_to_first_segments,
            max_parallel_repairs: dto.max_parallel_repairs,
            postprocess_timeout_ms: dto.postprocess_timeout_ms,
            size_increase: HlsSegmentRepairSizeIncreaseConfig::from(&dto.size_increase),
            corrupt_segment_watchdog: HlsCorruptSegmentWatchdogConfig::from(&dto.corrupt_segment_watchdog),
        }
    }
}

impl From<&HlsSegmentRepairConfig> for HlsSegmentRepairConfigDto {
    fn from(config: &HlsSegmentRepairConfig) -> Self {
        Self {
            max_level: config.max_level,
            apply_to_first_segments: config.apply_to_first_segments,
            max_parallel_repairs: config.max_parallel_repairs,
            postprocess_timeout_ms: config.postprocess_timeout_ms,
            size_increase: HlsSegmentRepairSizeIncreaseConfigDto::from(&config.size_increase),
            corrupt_segment_watchdog: HlsCorruptSegmentWatchdogConfigDto::from(&config.corrupt_segment_watchdog),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsCorruptSegmentWatchdogConfig {
    pub mode: shared::model::HlsCorruptSegmentWatchdogMode,
    pub max_parallel_jobs: usize,
}

impl Default for HlsCorruptSegmentWatchdogConfig {
    fn default() -> Self { Self::from(&HlsCorruptSegmentWatchdogConfigDto::default()) }
}

impl From<&HlsCorruptSegmentWatchdogConfigDto> for HlsCorruptSegmentWatchdogConfig {
    fn from(dto: &HlsCorruptSegmentWatchdogConfigDto) -> Self {
        Self {
            mode: dto.mode,
            max_parallel_jobs: dto.max_parallel_jobs,
        }
    }
}

impl From<&HlsCorruptSegmentWatchdogConfig> for HlsCorruptSegmentWatchdogConfigDto {
    fn from(config: &HlsCorruptSegmentWatchdogConfig) -> Self {
        Self {
            mode: config.mode,
            max_parallel_jobs: config.max_parallel_jobs,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsCacheConfig {
    pub cache_path: String,
    pub strip: StripConfig,
    pub cache_duration: u64,
    pub cache_bytes: u64,
    pub cache_bytes_str: String,
    pub cache_bytes_per_session: u64,
    pub cache_bytes_per_session_str: String,
    pub max_segments_prefetch: usize,
    pub max_concurrent_segment_fetches_per_session: usize,
    pub max_concurrent_segment_fetches_global: usize,
    pub origin_manifest_timeout_ms: u64,
    pub origin_segment_timeout_ms: u64,
    pub session_idle_timeout: u64,
    pub manifest_recovery_burst: HlsManifestRecoveryBurstConfig,
    pub segment_repair: HlsSegmentRepairConfig,
}

fn parse_hls_byte_size_or_default(value: &shared::model::ByteSize, default_value: &str) -> u64 {
    value.parse_bytes().unwrap_or_else(|_| {
        shared::model::ByteSize::new(default_value)
            .parse_bytes()
            .unwrap_or_default()
    })
}

pub fn default_hls_cache_path() -> String {
    std::env::temp_dir().join(HLS_CACHE_DIR_SUFFIX).to_string_lossy().into_owned()
}

impl From<&HlsCacheConfigDto> for HlsCacheConfig {
    fn from(dto: &HlsCacheConfigDto) -> Self {
        Self {
            cache_path: dto.cache_path.as_ref().map_or_else(default_hls_cache_path, |path: &String| Clone::clone(path)),
            strip: StripConfig::from(&dto.strip),
            cache_duration: dto.cache_duration,
            cache_bytes: parse_hls_byte_size_or_default(&dto.cache_bytes, "10GB"),
            cache_bytes_str: dto.cache_bytes.as_str().to_string(),
            cache_bytes_per_session: parse_hls_byte_size_or_default(&dto.cache_bytes_per_session, "512MB"),
            cache_bytes_per_session_str: dto.cache_bytes_per_session.as_str().to_string(),
            max_segments_prefetch: dto.max_segments_prefetch,
            max_concurrent_segment_fetches_per_session: dto.max_concurrent_segment_fetches_per_session,
            max_concurrent_segment_fetches_global: dto.max_concurrent_segment_fetches_global,
            origin_manifest_timeout_ms: dto.origin_manifest_timeout_ms,
            origin_segment_timeout_ms: dto.origin_segment_timeout_ms,
            session_idle_timeout: dto.session_idle_timeout,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::from(&dto.manifest_recovery_burst),
            segment_repair: HlsSegmentRepairConfig::from(&dto.segment_repair),
        }
    }
}

impl From<&HlsCacheConfig> for HlsCacheConfigDto {
    fn from(config: &HlsCacheConfig) -> Self {
        Self {
            cache_path: Some(config.cache_path.clone()),
            strip: shared::model::HlsStripConfigDto::from(&config.strip),
            cache_duration: config.cache_duration,
            cache_bytes: shared::model::ByteSize::new(config.cache_bytes_str.clone()),
            cache_bytes_per_session: shared::model::ByteSize::new(config.cache_bytes_per_session_str.clone()),
            max_segments_prefetch: config.max_segments_prefetch,
            max_concurrent_segment_fetches_per_session: config.max_concurrent_segment_fetches_per_session,
            max_concurrent_segment_fetches_global: config.max_concurrent_segment_fetches_global,
            origin_manifest_timeout_ms: config.origin_manifest_timeout_ms,
            origin_segment_timeout_ms: config.origin_segment_timeout_ms,
            session_idle_timeout: config.session_idle_timeout,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfigDto::from(&config.manifest_recovery_burst),
            segment_repair: HlsSegmentRepairConfigDto::from(&config.segment_repair),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReverseProxyConfig {
    pub resource_rewrite_disabled: bool,
    pub rewrite_secret: [u8; 16],
    pub resource_retry: ResourceRetryConfig,
    pub disabled_header: Option<ReverseProxyDisabledHeaderConfig>,
    pub stream: Option<StreamConfig>,
    pub cache: Option<CacheConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub geoip: Option<GeoIpConfig>,
    pub stream_history: Option<crate::model::StreamHistoryConfig>,
    pub qos_aggregation: Option<QosAggregationConfig>,
    pub hls_cache: Option<HlsCacheConfig>,
}

macros::from_impl!(ReverseProxyConfig);

impl From<&ReverseProxyConfigDto> for ReverseProxyConfig {
    fn from(dto: &ReverseProxyConfigDto) -> Self {
        Self {
            resource_rewrite_disabled: dto.resource_rewrite_disabled,
            rewrite_secret: hex_to_u8_16(&dto.rewrite_secret).unwrap_or_default(),
            resource_retry: dto
                .resource_retry
                .as_ref()
                .map_or_else(ResourceRetryConfig::default, Into::into),
            disabled_header: dto.disabled_header.as_ref().map(|d| ReverseProxyDisabledHeaderConfig {
                referer_header: d.referer_header,
                x_header: d.x_header,
                cloudflare_header: d.cloudflare_header,
                custom_header: d.custom_header.clone(),
            }),
            stream: dto.stream.as_ref().map(Into::into),
            cache: dto.cache.as_ref().map(Into::into),
            rate_limit: dto.rate_limit.as_ref().map(Into::into),
            geoip: dto.geoip.as_ref().map(Into::into),
            stream_history: dto.stream_history.as_ref().map(Into::into),
            qos_aggregation: dto.qos_aggregation.as_ref().map(Into::into),
            hls_cache: dto.hls_cache.as_ref().map(Into::into),
        }
    }
}

impl From<&ReverseProxyConfig> for ReverseProxyConfigDto {
    fn from(instance: &ReverseProxyConfig) -> Self {
        Self {
            resource_rewrite_disabled: instance.resource_rewrite_disabled,
            rewrite_secret: u8_16_to_hex(&instance.rewrite_secret),
            resource_retry: Some(ResourceRetryConfigDto::from(&instance.resource_retry)),
            disabled_header: instance.disabled_header.as_ref().map(|d| ReverseProxyDisabledHeaderConfigDto {
                referer_header: d.referer_header,
                x_header: d.x_header,
                cloudflare_header: d.cloudflare_header,
                custom_header: d.custom_header.clone(),
            }),
            stream: instance.stream.as_ref().map(Into::into),
            cache: instance.cache.as_ref().map(Into::into),
            rate_limit: instance.rate_limit.as_ref().map(Into::into),
            geoip: instance.geoip.as_ref().map(Into::into),
            stream_history: instance.stream_history.as_ref().map(Into::into),
            qos_aggregation: instance.qos_aggregation.as_ref().map(Into::into),
            hls_cache: instance.hls_cache.as_ref().map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        default_hls_cache_path, HlsCacheConfig, HlsManifestRecoveryBurstConfig, HlsSegmentRepairConfig,
        ReverseProxyConfig,
    };
    use shared::model::{ByteSize, HlsCacheConfigDto, HlsManifestRecoveryBurstLevel, HlsSegmentRepairMode, HlsStripMode, QosAggregationConfigDto, ReverseProxyConfigDto, StreamHistoryConfigDto};

    #[test]
    fn reverse_proxy_config_preserves_nested_stream_history() {
        let dto = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            stream_history: Some(StreamHistoryConfigDto {
                stream_history_enabled: true,
                stream_history_batch_size: 64,
                stream_history_retention_days: 14,
                stream_history_directory: "/var/lib/tuliprox/history".to_string(),
            }),
            qos_aggregation: None,
            ..Default::default()
        };

        let config = ReverseProxyConfig::from(&dto);
        let stream_history = config.stream_history.expect("stream history should exist");
        assert!(stream_history.stream_history_enabled);
        assert_eq!(stream_history.stream_history_batch_size, 64);
        assert_eq!(stream_history.stream_history_retention_days, 14);
        assert_eq!(stream_history.stream_history_directory, "/var/lib/tuliprox/history");
    }

    #[test]
    fn reverse_proxy_config_preserves_nested_qos_aggregation() {
        let dto = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            stream_history: Some(StreamHistoryConfigDto {
                stream_history_enabled: true,
                ..Default::default()
            }),
            qos_aggregation: Some(QosAggregationConfigDto {
                enabled: true,
                interval_secs: 300,
                ..Default::default()
            }),
            ..Default::default()
        };

        let config = ReverseProxyConfig::from(&dto);
        let qos = config.qos_aggregation.expect("qos_aggregation should exist");
        assert!(qos.enabled);
        assert_eq!(qos.interval_secs, 300);
    }

    #[test]
    fn manifest_recovery_beast_burst_uses_two_lanes_per_aggressive_slot() {
        let plan = HlsManifestRecoveryBurstLevel::Beast.plan();

        assert_eq!(plan.slots, 6);
        assert_eq!(plan.lanes_per_slot, 2);
        assert_eq!(plan.total_candidates(), plan.slots.saturating_mul(plan.lanes_per_slot));
        assert_eq!(plan.slot_for_candidate(0), 0);
        assert_eq!(plan.slot_for_candidate(1), 0);
        assert_eq!(plan.slot_for_candidate(2), 1);
        assert_eq!(plan.slot_for_candidate(plan.total_candidates().saturating_sub(1)), plan.slots.saturating_sub(1));
    }

    #[test]
    fn reverse_proxy_config_preserves_default_hls_cache_settings() {
        let dto = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto::default()),
            ..Default::default()
        };

        let config = ReverseProxyConfig::from(&dto);
        let hls = config.hls_cache.expect("hls_cache should exist");

        assert_eq!(
            hls,
            HlsCacheConfig {
                cache_path: default_hls_cache_path(),
                strip: super::StripConfig {
                    mode: HlsStripMode::Segments,
                    value: 0,
                },
                cache_duration: 300,
                cache_bytes: 10_737_418_240, // 10 * 1024^3 (was 10 * 1e9 under SI decimal before consolidation)
                cache_bytes_str: "10GB".to_string(),
                cache_bytes_per_session: 536_870_912, // 512 * 1024^2 (was 512 * 1e6 under SI decimal before consolidation)
                cache_bytes_per_session_str: "512MB".to_string(),
                max_segments_prefetch: 6,
                max_concurrent_segment_fetches_per_session: 2,
                max_concurrent_segment_fetches_global: 64,
                origin_manifest_timeout_ms: 3_000,
                origin_segment_timeout_ms: 10_000,
                session_idle_timeout: 300,
                manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
                segment_repair: HlsSegmentRepairConfig {
                    max_level: HlsSegmentRepairMode::Off,
                    apply_to_first_segments: 1,
                    max_parallel_repairs: 1,
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn hls_cache_runtime_config_parses_byte_sizes() {
        let dto = HlsCacheConfigDto {
            cache_bytes: ByteSize::new("1GiB"),
            cache_bytes_per_session: ByteSize::new("512MB"),
            ..Default::default()
        };

        let config = HlsCacheConfig::from(&dto);

        assert_eq!(config.cache_bytes, 1_073_741_824);
        assert_eq!(config.cache_bytes_per_session, 536_870_912); // 512 * 1024^2 (binary since parse_size_base_2 consolidation)
    }

    #[test]
    fn hls_cache_runtime_config_roundtrips_human_readable_sizes() {
        let dto = HlsCacheConfigDto {
            cache_bytes: ByteSize::new("1GiB"),
            cache_bytes_per_session: ByteSize::new("512MB"),
            ..Default::default()
        };

        let config = HlsCacheConfig::from(&dto);
        let roundtrip = HlsCacheConfigDto::from(&config);

        assert_eq!(roundtrip.cache_bytes.as_str(), "1GiB");
        assert_eq!(roundtrip.cache_bytes_per_session.as_str(), "512MB");
    }
}
