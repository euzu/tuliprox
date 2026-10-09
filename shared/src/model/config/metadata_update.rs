use crate::defaults::{
    default_metadata_max_queue_size, default_metadata_no_change_cache_ttl_secs, default_metadata_path,
    default_metadata_probe_fairness_resolve_burst, default_metadata_retry_delay, default_metadata_worker_idle_timeout,
    is_default_metadata_max_queue_size, is_default_metadata_no_change_cache_ttl_secs, is_default_metadata_path,
    is_default_metadata_probe_fairness_resolve_burst, is_default_metadata_retry_delay,
    is_default_metadata_worker_idle_timeout,
};

const MIN_DURATION_SECS: u64 = 1;

const MIN_ATTEMPTS: u8 = 1;

const MAX_JITTER_PERCENT: u8 = 95;

const MIN_QUEUE_SIZE: usize = 1;

const DEFAULT_FFPROBE_TIMEOUT_SECS: u64 = 60;

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
    #[serde(
        default = "default_metadata_no_change_cache_ttl_secs",
        skip_serializing_if = "is_default_metadata_no_change_cache_ttl_secs"
    )]
    pub no_change_cache_ttl_secs: u64,
    #[serde(
        default = "default_metadata_probe_fairness_resolve_burst",
        skip_serializing_if = "is_default_metadata_probe_fairness_resolve_burst"
    )]
    pub probe_fairness_resolve_burst: usize,
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
            no_change_cache_ttl_secs: default_metadata_no_change_cache_ttl_secs(),
            probe_fairness_resolve_burst: default_metadata_probe_fairness_resolve_burst(),
        }
    }
}

#[cfg(test)]
mod tests;

mod normalize;
mod policies;
mod providers;
pub use policies::{MetadataLogConfigDto, ProbeConfigDto, ResolveConfigDto};
pub use providers::{FfprobeConfigDto, TmdbConfigDto};
