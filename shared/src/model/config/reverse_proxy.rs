use crate::{
    error::TuliproxError,
    model::{
        CacheConfigDto, GeoIpConfigDto, QosAggregationConfigDto, RateLimitConfigDto, StreamConfigDto,
        StreamHistoryConfigDto,
    },
    utils::{
        default_resource_retry_attempts, default_resource_retry_backoff_ms, default_resource_retry_backoff_multiplier,
        hex_to_u8_16, is_default_resource_retry_attempts, is_default_resource_retry_backoff_ms,
        is_default_resource_retry_backoff_multiplier, is_empty_optional_vec, is_false,
    },
};
use log::warn;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

const DEFAULT_HLS_CACHE_PATH: &str = "/tmp/tuliprox/cache/hls";
const DEFAULT_HLS_CACHE_BYTES: &str = "10GB";
const DEFAULT_HLS_CACHE_BYTES_PER_SESSION: &str = "512MB";

fn default_hls_cache_path() -> String { DEFAULT_HLS_CACHE_PATH.to_string() }
const fn default_hls_cache_duration() -> u64 { 300 }
fn default_hls_cache_bytes() -> ByteSizeDto { ByteSizeDto(DEFAULT_HLS_CACHE_BYTES.to_string()) }
fn default_hls_cache_bytes_per_session() -> ByteSizeDto { ByteSizeDto(DEFAULT_HLS_CACHE_BYTES_PER_SESSION.to_string()) }
const fn default_hls_max_segments_prefetch() -> usize { 6 }
const fn default_hls_max_concurrent_segment_fetches_per_session() -> usize { 2 }
const fn default_hls_max_concurrent_segment_fetches_global() -> usize { 64 }
const fn default_hls_origin_manifest_timeout_ms() -> u64 { 3_000 }
const fn default_hls_origin_segment_timeout_ms() -> u64 { 10_000 }
const fn default_hls_session_idle_timeout() -> u64 { 300 }
const fn default_hls_segment_repair_apply_to_first_segments() -> u8 { 1 }
const fn default_hls_segment_repair_max_parallel_repairs() -> usize { 1 }
const fn default_hls_segment_repair_low_size_increase_percent() -> u8 { 2 }
const fn default_hls_segment_repair_medium_size_increase_percent() -> u8 { 5 }
const fn default_hls_segment_repair_high_size_increase_percent() -> u8 { 20 }
const fn default_hls_segment_repair_postprocess_timeout_ms() -> u64 { 2_000 }
const fn default_hls_corrupt_segment_watchdog_max_parallel_jobs() -> usize { 1 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteSizeDto(String);

impl ByteSizeDto {
    pub fn new(value: impl Into<String>) -> Self { Self(value.into()) }

    pub fn as_str(&self) -> &str { self.0.as_str() }

    pub fn clean_or_default(&mut self, default_value: &str) {
        let trimmed = self.0.trim();
        self.0 = if trimmed.is_empty() { default_value.to_string() } else { trimmed.to_string() };
    }

    pub fn parse_bytes(&self) -> Result<u64, String> { parse_hls_byte_size(self.0.as_str()) }
}

impl From<String> for ByteSizeDto {
    fn from(value: String) -> Self { Self::new(value) }
}

impl From<&str> for ByteSizeDto {
    fn from(value: &str) -> Self { Self::new(value) }
}

impl std::fmt::Display for ByteSizeDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

impl Serialize for ByteSizeDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for ByteSizeDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ByteSizeVisitor;

        impl<'de> de::Visitor<'de> for ByteSizeVisitor {
            type Value = ByteSizeDto;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a byte size string or unsigned integer")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ByteSizeDto(value.to_string()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ByteSizeDto(value))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ByteSizeDto(value.to_string()))
            }
        }

        deserializer.deserialize_any(ByteSizeVisitor)
    }
}

fn parse_hls_byte_size(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let split_at = trimmed.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(trimmed.len());
    let number_part = &trimmed[..split_at];
    let suffix_part = trimmed[split_at..].trim();

    if number_part.is_empty() {
        return Err(format!("Invalid byte size: {value}"));
    }

    let number = number_part.parse::<u64>().map_err(|_| format!("Invalid byte size: {value}"))?;

    let multiplier = match suffix_part {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KiB" => 1024,
        "MiB" => 1024 * 1024,
        "GiB" => 1024 * 1024 * 1024,
        "TiB" => 1024_u64.pow(4),
        _ => return Err(format!("Unknown byte size unit: {suffix_part}")),
    };

    number.checked_mul(multiplier).ok_or_else(|| format!("Byte size too large: {value}"))
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StripModeDto {
    #[default]
    Segments,
    Seconds,
}

impl std::fmt::Display for StripModeDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Segments => f.write_str("segments"),
            Self::Seconds => f.write_str("seconds"),
        }
    }
}

impl std::str::FromStr for StripModeDto {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "segments" => Ok(Self::Segments),
            "seconds" => Ok(Self::Seconds),
            _ => Err(format!("Unknown HLS strip mode: {value}")),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StripConfigDto {
    #[serde(default)]
    pub mode: StripModeDto,
    #[serde(default)]
    pub value: u64,
}

impl StripConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub const fn clean(&mut self) {}
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HlsSegmentRepairModeDto {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl std::fmt::Display for HlsSegmentRepairModeDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Low => f.write_str("low"),
            Self::Medium => f.write_str("medium"),
            Self::High => f.write_str("high"),
        }
    }
}

impl std::str::FromStr for HlsSegmentRepairModeDto {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(format!("Unknown HLS segment repair mode: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HlsCorruptSegmentWatchdogModeDto {
    #[default]
    Off,
    DetectOnly,
    Sanitize,
    Diagnostic,
}

impl std::fmt::Display for HlsCorruptSegmentWatchdogModeDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::DetectOnly => f.write_str("detect_only"),
            Self::Sanitize => f.write_str("sanitize"),
            Self::Diagnostic => f.write_str("diagnostic"),
        }
    }
}

impl std::str::FromStr for HlsCorruptSegmentWatchdogModeDto {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "detect_only" => Ok(Self::DetectOnly),
            "sanitize" => Ok(Self::Sanitize),
            "diagnostic" => Ok(Self::Diagnostic),
            _ => Err(format!("Unknown HLS corrupt segment watchdog mode: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HlsManifestRecoveryBurstLevelDto {
    #[default]
    Off,
    Friendly,
    Cautious,
    Balanced,
    Intense,
    Aggressive,
    Beast,
}

impl std::fmt::Display for HlsManifestRecoveryBurstLevelDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Friendly => f.write_str("friendly"),
            Self::Cautious => f.write_str("cautious"),
            Self::Balanced => f.write_str("balanced"),
            Self::Intense => f.write_str("intense"),
            Self::Aggressive => f.write_str("aggressive"),
            Self::Beast => f.write_str("beast"),
        }
    }
}

impl std::str::FromStr for HlsManifestRecoveryBurstLevelDto {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "friendly" => Ok(Self::Friendly),
            "cautious" => Ok(Self::Cautious),
            "balanced" => Ok(Self::Balanced),
            "intense" => Ok(Self::Intense),
            "aggressive" => Ok(Self::Aggressive),
            "beast" => Ok(Self::Beast),
            _ => Err(format!("Unknown HLS manifest recovery burst level: {value}")),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HlsManifestRecoveryBurstConfigDto {
    #[serde(default)]
    pub level: HlsManifestRecoveryBurstLevelDto,
}

impl HlsManifestRecoveryBurstConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

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
    pub fn is_empty(&self) -> bool { self == &Self::default() }

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
    pub max_level: HlsSegmentRepairModeDto,
    #[serde(default = "default_hls_segment_repair_apply_to_first_segments")]
    pub apply_to_first_segments: u8,
    #[serde(default = "default_hls_segment_repair_max_parallel_repairs")]
    pub max_parallel_repairs: usize,
    #[serde(default = "default_hls_segment_repair_postprocess_timeout_ms")]
    pub postprocess_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "HlsSegmentRepairSizeIncreaseConfigDto::is_empty")]
    pub size_increase: HlsSegmentRepairSizeIncreaseConfigDto,
    #[serde(default, skip_serializing_if = "HlsCorruptSegmentWatchdogConfigDto::is_empty")]
    pub corrupt_segment_watchdog: HlsCorruptSegmentWatchdogConfigDto,
}

impl Default for HlsSegmentRepairConfigDto {
    fn default() -> Self {
        Self {
            max_level: HlsSegmentRepairModeDto::Off,
            apply_to_first_segments: default_hls_segment_repair_apply_to_first_segments(),
            max_parallel_repairs: default_hls_segment_repair_max_parallel_repairs(),
            postprocess_timeout_ms: default_hls_segment_repair_postprocess_timeout_ms(),
            size_increase: HlsSegmentRepairSizeIncreaseConfigDto::default(),
            corrupt_segment_watchdog: HlsCorruptSegmentWatchdogConfigDto::default(),
        }
    }
}

impl HlsSegmentRepairConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub fn clean(&mut self) {
        self.size_increase.clean();
        self.corrupt_segment_watchdog.clean();
    }

    fn validate(&self) -> Result<(), TuliproxError> {
        if self.postprocess_timeout_ms < 100 {
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
    pub mode: HlsCorruptSegmentWatchdogModeDto,
    #[serde(default = "default_hls_corrupt_segment_watchdog_max_parallel_jobs")]
    pub max_parallel_jobs: usize,
}

impl Default for HlsCorruptSegmentWatchdogConfigDto {
    fn default() -> Self {
        Self {
            mode: HlsCorruptSegmentWatchdogModeDto::Off,
            max_parallel_jobs: default_hls_corrupt_segment_watchdog_max_parallel_jobs(),
        }
    }
}

impl HlsCorruptSegmentWatchdogConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

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
    #[serde(default = "default_hls_cache_path")]
    pub cache_path: String,
    #[serde(default)]
    pub strip: StripConfigDto,
    #[serde(default = "default_hls_cache_duration")]
    pub cache_duration: u64,
    #[serde(default = "default_hls_cache_bytes")]
    pub cache_bytes: ByteSizeDto,
    #[serde(default = "default_hls_cache_bytes_per_session")]
    pub cache_bytes_per_session: ByteSizeDto,
    #[serde(default = "default_hls_max_segments_prefetch")]
    pub max_segments_prefetch: usize,
    #[serde(default = "default_hls_max_concurrent_segment_fetches_per_session")]
    pub max_concurrent_segment_fetches_per_session: usize,
    #[serde(default = "default_hls_max_concurrent_segment_fetches_global")]
    pub max_concurrent_segment_fetches_global: usize,
    #[serde(default = "default_hls_origin_manifest_timeout_ms")]
    pub origin_manifest_timeout_ms: u64,
    #[serde(default = "default_hls_origin_segment_timeout_ms")]
    pub origin_segment_timeout_ms: u64,
    #[serde(default = "default_hls_session_idle_timeout")]
    pub session_idle_timeout: u64,
    #[serde(default, skip_serializing_if = "HlsManifestRecoveryBurstConfigDto::is_empty")]
    pub manifest_recovery_burst: HlsManifestRecoveryBurstConfigDto,
    #[serde(default, skip_serializing_if = "HlsSegmentRepairConfigDto::is_empty")]
    pub segment_repair: HlsSegmentRepairConfigDto,
}

impl Default for HlsCacheConfigDto {
    fn default() -> Self {
        Self {
            cache_path: default_hls_cache_path(),
            strip: StripConfigDto::default(),
            cache_duration: default_hls_cache_duration(),
            cache_bytes: default_hls_cache_bytes(),
            cache_bytes_per_session: default_hls_cache_bytes_per_session(),
            max_segments_prefetch: default_hls_max_segments_prefetch(),
            max_concurrent_segment_fetches_per_session: default_hls_max_concurrent_segment_fetches_per_session(),
            max_concurrent_segment_fetches_global: default_hls_max_concurrent_segment_fetches_global(),
            origin_manifest_timeout_ms: default_hls_origin_manifest_timeout_ms(),
            origin_segment_timeout_ms: default_hls_origin_segment_timeout_ms(),
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

    fn ensure_min_u64(field_name: &str, value: u64, min_value: u64) -> Result<(), TuliproxError> {
        if value < min_value {
            return Err(TuliproxError::ConfigReverseProxy(format!("hls_cache.{field_name} must be >= {min_value}")));
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
        let cache_path = self.cache_path.trim();
        self.cache_path = if cache_path.is_empty() { default_hls_cache_path() } else { cache_path.to_string() };

        self.cache_bytes.clean_or_default(DEFAULT_HLS_CACHE_BYTES);
        self.cache_bytes_per_session.clean_or_default(DEFAULT_HLS_CACHE_BYTES_PER_SESSION);

        self.cache_bytes.parse_bytes().map_err(TuliproxError::ConfigReverseProxy)?;
        self.cache_bytes_per_session.parse_bytes().map_err(TuliproxError::ConfigReverseProxy)?;

        Self::ensure_min_u64("cache_duration", self.cache_duration, 1)?;
        Self::ensure_min_usize(
            "max_concurrent_segment_fetches_per_session",
            self.max_concurrent_segment_fetches_per_session,
            1,
        )?;
        Self::ensure_min_usize("max_concurrent_segment_fetches_global", self.max_concurrent_segment_fetches_global, 1)?;
        Self::ensure_min_u64("origin_manifest_timeout_ms", self.origin_manifest_timeout_ms, 1)?;
        Self::ensure_min_u64("origin_segment_timeout_ms", self.origin_segment_timeout_ms, 1)?;
        Self::ensure_min_u64("session_idle_timeout", self.session_idle_timeout, 1)?;
        if self.segment_repair.apply_to_first_segments > 6 {
            return Err(TuliproxError::ConfigReverseProxy(
                "hls_cache.segment_repair.apply_to_first_segments must be <= 6".to_string(),
            ));
        }
        self.segment_repair.validate()?;
        if self.segment_repair.max_level != HlsSegmentRepairModeDto::Off
            && self.segment_repair.max_parallel_repairs == 0
        {
            return Err(TuliproxError::ConfigReverseProxy(
                "hls_cache.segment_repair.max_parallel_repairs must be >= 1 when segment repair is enabled".to_string(),
            ));
        }
        if self.segment_repair.max_level != HlsSegmentRepairModeDto::Off
            && self.segment_repair.max_parallel_repairs > self.max_segments_prefetch
        {
            return Err(TuliproxError::ConfigReverseProxy(format!(
                "hls_cache.segment_repair.max_parallel_repairs must be <= max_segments_prefetch ({})",
                self.max_segments_prefetch
            )));
        }
        if self.segment_repair.corrupt_segment_watchdog.mode != HlsCorruptSegmentWatchdogModeDto::Off
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReverseProxyDisabledHeaderConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub referer_header: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub x_header: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cloudflare_header: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_header: Vec<String>,
}

impl ReverseProxyDisabledHeaderConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.referer_header
            && !self.x_header
            && !self.cloudflare_header
            && self.custom_header.iter().all(|h| h.trim().is_empty())
    }

    pub fn clean(&mut self) { self.custom_header.retain(|h| !h.trim().is_empty()); }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReverseProxyConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub resource_rewrite_disabled: bool,
    pub rewrite_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_retry: Option<ResourceRetryConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_header: Option<ReverseProxyDisabledHeaderConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geoip: Option<GeoIpConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_history: Option<StreamHistoryConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos_aggregation: Option<QosAggregationConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hls_cache: Option<HlsCacheConfigDto>,
}

impl ReverseProxyConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.resource_rewrite_disabled
            && self.disabled_header.as_ref().is_none_or(|d| d.is_empty())
            && self.resource_retry.as_ref().is_none_or(ResourceRetryConfigDto::is_default)
            && (self.stream.is_none() || self.stream.as_ref().is_some_and(|s| s.is_empty()))
            && (self.cache.is_none() || self.cache.as_ref().is_some_and(|c| c.is_empty()))
            && (self.rate_limit.is_none() || self.rate_limit.as_ref().is_some_and(|r| r.is_empty()))
            && (self.geoip.is_none() || self.geoip.as_ref().is_some_and(|g| g.is_empty()))
            && (self.stream_history.is_none() || self.stream_history.as_ref().is_some_and(|s| s.is_empty()))
            && (self.qos_aggregation.is_none()
                || self.qos_aggregation.as_ref().is_some_and(QosAggregationConfigDto::is_empty))
            && (self.hls_cache.is_none() || self.hls_cache.as_ref().is_some_and(HlsCacheConfigDto::is_empty))
    }

    pub fn clean(&mut self) {
        if let Some(disabled) = self.disabled_header.as_mut() {
            disabled.clean();
            if disabled.is_empty() {
                self.disabled_header = None;
            }
        }
        if self.resource_retry.as_ref().is_some_and(ResourceRetryConfigDto::is_default) {
            self.resource_retry = None;
        }
        if self.stream.as_ref().is_some_and(StreamConfigDto::is_empty) {
            self.stream = None;
        }
        if self.cache.as_ref().is_some_and(CacheConfigDto::is_empty) {
            self.cache = None;
        }
        if self.rate_limit.as_ref().is_some_and(RateLimitConfigDto::is_empty) {
            self.rate_limit = None;
        }
        if self.geoip.as_ref().is_some_and(GeoIpConfigDto::is_empty) {
            self.geoip = None;
        }
        if self.stream_history.as_ref().is_some_and(StreamHistoryConfigDto::is_empty) {
            self.stream_history = None;
        }
        if self.qos_aggregation.as_ref().is_some_and(QosAggregationConfigDto::is_empty) {
            self.qos_aggregation = None;
        }
        if let Some(hls_cache) = self.hls_cache.as_mut() {
            hls_cache.clean();
            if hls_cache.is_empty() {
                self.hls_cache = None;
            }
        }
    }

    pub(crate) fn prepare(&mut self, storage_dir: &str) -> Result<(), TuliproxError> {
        self.rewrite_secret = self.rewrite_secret.trim().to_string();
        if !self.resource_rewrite_disabled {
            if self.rewrite_secret.is_empty() {
                return Err(TuliproxError::ConfigReverseProxy(
                    "rewrite_secret is required when resource rewrite is enabled".to_string(),
                ));
            }
            hex_to_u8_16(&self.rewrite_secret).map_err(TuliproxError::ConfigReverseProxy)?;
        }

        if let Some(stream) = self.stream.as_mut() {
            stream.prepare()?;
        }
        if let Some(cache) = self.cache.as_mut() {
            if cache.enabled && self.resource_rewrite_disabled {
                warn!("The cache is disabled because resource rewrite is disabled");
                cache.enabled = false;
            }
            cache.prepare(storage_dir)?;
        }

        if let Some(rate_limit) = self.rate_limit.as_mut() {
            if rate_limit.enabled {
                rate_limit.prepare()?;
            }
        }

        let mut stream_history_enabled = false;
        if let Some(stream_history) = self.stream_history.as_mut() {
            stream_history.prepare(storage_dir)?;
            stream_history_enabled = stream_history.stream_history_enabled;
        }
        if let Some(qos_aggregation) = self.qos_aggregation.as_mut() {
            qos_aggregation.prepare(stream_history_enabled)?;
        }

        if let Some(resource_retry) = self.resource_retry.as_mut() {
            resource_retry.prepare()?;
        }
        if let Some(hls_cache) = self.hls_cache.as_mut() {
            hls_cache.prepare()?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResourceRetryConfigDto {
    #[serde(default = "default_resource_retry_attempts", skip_serializing_if = "is_default_resource_retry_attempts")]
    pub max_attempts: u32,
    #[serde(
        default = "default_resource_retry_backoff_ms",
        skip_serializing_if = "is_default_resource_retry_backoff_ms"
    )]
    pub backoff_millis: u64,
    #[serde(
        default = "default_resource_retry_backoff_multiplier",
        skip_serializing_if = "is_default_resource_retry_backoff_multiplier"
    )]
    pub backoff_multiplier: f64,
    #[serde(default, skip_serializing_if = "is_empty_optional_vec")]
    pub failover_redirect_patterns: Option<Vec<String>>,
}

impl Default for ResourceRetryConfigDto {
    fn default() -> Self {
        Self {
            max_attempts: default_resource_retry_attempts(),
            backoff_millis: default_resource_retry_backoff_ms(),
            backoff_multiplier: default_resource_retry_backoff_multiplier(),
            failover_redirect_patterns: None,
        }
    }
}

impl ResourceRetryConfigDto {
    pub fn is_default(&self) -> bool {
        self.max_attempts == default_resource_retry_attempts()
            && self.backoff_millis == default_resource_retry_backoff_ms()
            && (self.backoff_multiplier - default_resource_retry_backoff_multiplier()).abs() < f64::EPSILON
            && is_empty_optional_vec(&self.failover_redirect_patterns)
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if let Some(failover_redirect_patterns) = self.failover_redirect_patterns.as_mut() {
            for pattern in failover_redirect_patterns {
                if let Err(err) = crate::model::REGEX_CACHE.get_or_compile(pattern) {
                    return Err(TuliproxError::RegexCompile(format!("{pattern} {err}")));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ByteSizeDto, HlsCacheConfigDto, HlsCorruptSegmentWatchdogConfigDto, HlsCorruptSegmentWatchdogModeDto,
        HlsManifestRecoveryBurstConfigDto, HlsManifestRecoveryBurstLevelDto, HlsSegmentRepairConfigDto,
        HlsSegmentRepairModeDto, HlsSegmentRepairSizeIncreaseConfigDto, ReverseProxyConfigDto,
    };
    use crate::model::{QosAggregationConfigDto, StreamHistoryConfigDto};

    #[test]
    fn serializing_stream_history_under_reverse_proxy_uses_nested_yaml_shape() {
        let cfg = ReverseProxyConfigDto {
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

        let serialized = serde_saphyr::to_string(&cfg).expect("serialization should succeed");
        assert!(serialized.contains("stream_history:"), "expected nested stream_history block, got: {serialized}");
    }

    #[test]
    fn prepare_uses_default_directory_when_stream_history_directory_is_blank() {
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            stream_history: Some(StreamHistoryConfigDto {
                stream_history_enabled: true,
                stream_history_batch_size: 64,
                stream_history_retention_days: 14,
                stream_history_directory: String::new(),
            }),
            ..Default::default()
        };

        cfg.prepare("storage").expect("prepare should succeed with blank directory");
        let sh = cfg.stream_history.as_ref().unwrap();
        // Blank directory must resolve to an absolute path ending with the default subdir name.
        assert!(
            sh.stream_history_directory.ends_with("stream_history"),
            "expected default subdir 'stream_history', got: {}",
            sh.stream_history_directory
        );
        assert!(
            std::path::Path::new(&sh.stream_history_directory).is_absolute(),
            "expected absolute path, got: {}",
            sh.stream_history_directory
        );
    }

    #[test]
    fn prepare_normalizes_relative_stream_history_directory_against_storage_dir() {
        use std::path::{Path, PathBuf};

        let storage_dir = if cfg!(windows) { r"C:\data\tuliprox" } else { "/var/lib/tuliprox" };
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            stream_history: Some(StreamHistoryConfigDto {
                stream_history_enabled: true,
                stream_history_batch_size: 64,
                stream_history_retention_days: 14,
                stream_history_directory: "history".to_string(),
            }),
            ..Default::default()
        };

        cfg.prepare(storage_dir).expect("prepare should succeed");

        let stream_history = cfg.stream_history.expect("stream history should exist");
        let expected = PathBuf::from(storage_dir).join("history");
        let actual = Path::new(&stream_history.stream_history_directory);
        assert_eq!(actual.file_name(), expected.file_name());
        assert!(actual.ends_with("history"), "directory should end with 'history', got: {actual:?}");
    }

    #[test]
    fn qos_aggregation_deserializes_under_reverse_proxy() {
        let yaml = r#"
rewrite_secret: 00112233445566778899aabbccddeeff
stream_history:
  stream_history_enabled: true
qos_aggregation:
  enabled: true
  interval_secs: 300
"#;

        let cfg: ReverseProxyConfigDto = serde_saphyr::from_str(yaml).expect("reverse_proxy should deserialize");
        let qos = cfg.qos_aggregation.expect("qos_aggregation should deserialize");
        assert!(qos.enabled);
        assert_eq!(qos.interval_secs, 300);
    }

    #[test]
    fn prepare_disables_qos_aggregation_when_stream_history_is_disabled() {
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            qos_aggregation: Some(QosAggregationConfigDto { enabled: true, interval_secs: 300 }),
            ..Default::default()
        };

        cfg.prepare("storage").expect("prepare should succeed");
        let qos = cfg.qos_aggregation.expect("qos_aggregation should remain present");
        assert!(!qos.enabled, "qos_aggregation must disable itself when stream_history is disabled");
    }

    #[test]
    fn prepare_rejects_zero_qos_aggregation_interval_when_enabled() {
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            stream_history: Some(StreamHistoryConfigDto { stream_history_enabled: true, ..Default::default() }),
            qos_aggregation: Some(QosAggregationConfigDto { enabled: true, interval_secs: 0 }),
            ..Default::default()
        };

        let err = cfg.prepare("storage").expect_err("prepare must reject zero interval");
        assert!(err.to_string().contains("interval_secs"), "unexpected error: {err}");
    }

    #[test]
    fn hls_byte_size_parses_supported_units() {
        assert_eq!(ByteSizeDto::new("10GB").parse_bytes().expect("10GB should parse"), 10_000_000_000);
        assert_eq!(ByteSizeDto::new("512MB").parse_bytes().expect("512MB should parse"), 512_000_000);
        assert_eq!(ByteSizeDto::new("1048576").parse_bytes().expect("bytes should parse"), 1_048_576);
        assert_eq!(ByteSizeDto::new("1GiB").parse_bytes().expect("1GiB should parse"), 1_073_741_824);
        assert_eq!(ByteSizeDto::new("0").parse_bytes().expect("0 should parse"), 0);
    }

    #[test]
    fn hls_cache_deserializes_target_yaml() {
        let yaml = r#"
rewrite_secret: 00112233445566778899aabbccddeeff
hls_cache:
  cache_path: "/tmp/tuliprox/cache/hls"
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
        assert!(serialized.contains("cache_path:"), "expected cache_path field, got: {serialized}");
    }

    #[test]
    fn hls_cache_serializes_non_default_segment_repair() {
        let cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto {
                segment_repair: HlsSegmentRepairConfigDto {
                    max_level: HlsSegmentRepairModeDto::Medium,
                    apply_to_first_segments: 2,
                    max_parallel_repairs: 2,
                    postprocess_timeout_ms: 1_500,
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
                        mode: HlsCorruptSegmentWatchdogModeDto::Sanitize,
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
                    level: HlsManifestRecoveryBurstLevelDto::Beast,
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
        assert_eq!("beast".parse::<HlsManifestRecoveryBurstLevelDto>(), Ok(HlsManifestRecoveryBurstLevelDto::Beast));
    }

    #[test]
    fn hls_cache_rejects_corrupt_segment_watchdog_command_version() {
        let yaml = r#"
rewrite_secret: 00112233445566778899aabbccddeeff
hls_cache:
  segment_repair:
    corrupt_segment_watchdog:
      mode: sanitize
      command_version: 1
"#;

        let err = serde_saphyr::from_str::<ReverseProxyConfigDto>(yaml)
            .expect_err("command_version must be rejected as unknown config");
        assert!(err.to_string().contains("command_version"), "unexpected error: {err}");
    }

    #[test]
    fn hls_cache_serializes_non_default_segment_repair_size_increase() {
        let segment_repair = HlsSegmentRepairConfigDto {
            max_level: HlsSegmentRepairModeDto::Medium,
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
                max_level: HlsSegmentRepairModeDto::Medium,
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
            cache_path: " ".to_string(),
            cache_bytes: ByteSizeDto::new(" "),
            cache_bytes_per_session: ByteSizeDto::new(""),
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
            ("cache_duration", HlsCacheConfigDto { cache_duration: 0, ..Default::default() }),
            (
                "max_concurrent_segment_fetches_per_session",
                HlsCacheConfigDto { max_concurrent_segment_fetches_per_session: 0, ..Default::default() },
            ),
            (
                "max_concurrent_segment_fetches_global",
                HlsCacheConfigDto { max_concurrent_segment_fetches_global: 0, ..Default::default() },
            ),
            ("origin_manifest_timeout_ms", HlsCacheConfigDto { origin_manifest_timeout_ms: 0, ..Default::default() }),
            ("origin_segment_timeout_ms", HlsCacheConfigDto { origin_segment_timeout_ms: 0, ..Default::default() }),
            ("session_idle_timeout", HlsCacheConfigDto { session_idle_timeout: 0, ..Default::default() }),
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
        too_many_parallel_repairs.segment_repair.max_level = HlsSegmentRepairModeDto::Medium;
        too_many_parallel_repairs.segment_repair.max_parallel_repairs = 3;
        let err = too_many_parallel_repairs.prepare().expect_err("too many parallel repairs should be rejected");
        assert!(err.to_string().contains("segment_repair.max_parallel_repairs"), "unexpected error: {err}");

        let mut invalid_size_increase = HlsCacheConfigDto::default();
        invalid_size_increase.segment_repair.size_increase.high_percent = 101;
        let err = invalid_size_increase.prepare().expect_err("size increase above 100 should be rejected");
        assert!(err.to_string().contains("segment_repair.size_increase.high_percent"), "unexpected error: {err}");

        let mut invalid_postprocess_timeout = HlsCacheConfigDto::default();
        invalid_postprocess_timeout.segment_repair.postprocess_timeout_ms = 99;
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
        cfg.segment_repair.max_level = HlsSegmentRepairModeDto::Medium;
        cfg.segment_repair.max_parallel_repairs = 0;

        let err = cfg.prepare().expect_err("enabled repair needs a positive parallel limit");
        assert!(err.to_string().contains("segment_repair.max_parallel_repairs"), "unexpected error: {err}");
    }

    #[test]
    fn reverse_proxy_prepare_rejects_invalid_hls_cache_byte_size() {
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto { cache_bytes: ByteSizeDto::new("10XB"), ..Default::default() }),
            ..Default::default()
        };

        let err = cfg.prepare("storage").expect_err("invalid hls cache byte size must be rejected");

        assert!(err.to_string().contains("Unknown byte size unit"), "unexpected error: {err}");
    }

    #[test]
    fn reverse_proxy_clean_removes_default_hls_cache() {
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto::default()),
            ..Default::default()
        };

        cfg.clean();

        assert!(cfg.hls_cache.is_none());
    }

    #[test]
    fn reverse_proxy_clean_keeps_non_default_hls_segment_repair() {
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_cache: Some(HlsCacheConfigDto {
                segment_repair: HlsSegmentRepairConfigDto {
                    max_level: HlsSegmentRepairModeDto::Medium,
                    apply_to_first_segments: 1,
                    max_parallel_repairs: 1,
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        cfg.clean();

        let Some(hls_cache) = cfg.hls_cache else {
            panic!("non-default hls segment repair must keep hls_cache");
        };
        assert_eq!(hls_cache.segment_repair.max_level, HlsSegmentRepairModeDto::Medium);
        assert_eq!(hls_cache.segment_repair.apply_to_first_segments, 1);
        assert_eq!(hls_cache.segment_repair.max_parallel_repairs, 1);
    }
}
