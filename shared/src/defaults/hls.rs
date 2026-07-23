//! HLS-cache defaults: path, size budgets, timeouts, segment-repair knobs,
//! corrupt-segment watchdog, session TTLs, manifest-recovery burst sizes.

use crate::model::ByteSize;

default_eq_fns!(
    default_hls_session_ttl_secs, is_default_hls_session_ttl_secs, u64, 15;
);

// Resolved against the OS temp dir so the default works on Linux, macOS, and Windows.
pub const HLS_CACHE_DIR_SUFFIX: &str = "tuliprox/cache/hls";
pub const DEFAULT_HLS_CACHE_BYTES: &str = "10GB";
pub const DEFAULT_HLS_CACHE_BYTES_PER_SESSION: &str = "512MB";

pub const fn default_hls_cache_duration() -> u64 { 300 }
pub fn default_hls_cache_bytes() -> ByteSize { ByteSize::new(DEFAULT_HLS_CACHE_BYTES) }
pub fn default_hls_cache_bytes_per_session() -> ByteSize { ByteSize::new(DEFAULT_HLS_CACHE_BYTES_PER_SESSION) }
pub const fn default_hls_max_segments_prefetch() -> usize { 6 }
pub const fn default_hls_max_concurrent_segment_fetches_per_session() -> usize { 2 }
pub const fn default_hls_max_concurrent_segment_fetches_global() -> usize { 64 }
pub const fn default_hls_origin_manifest_timeout_ms() -> u64 { 3_000 }
pub const fn default_hls_origin_segment_timeout_ms() -> u64 { 10_000 }
pub const fn default_hls_session_idle_timeout() -> u64 { 300 }
pub const fn default_hls_segment_repair_apply_to_first_segments() -> u8 { 1 }
pub const fn default_hls_segment_repair_max_parallel_repairs() -> usize { 1 }
pub const fn default_hls_segment_repair_low_size_increase_percent() -> u8 { 2 }
pub const fn default_hls_segment_repair_medium_size_increase_percent() -> u8 { 5 }
pub const fn default_hls_segment_repair_high_size_increase_percent() -> u8 { 20 }
pub const fn default_hls_segment_repair_postprocess_timeout_ms() -> u64 { 2_000 }
pub const fn default_hls_corrupt_segment_watchdog_max_parallel_jobs() -> usize { 1 }

// HLS manifest / fallback filename constants.
pub const HLS_EXT: &str = ".m3u8";
pub const TS_EXT: &str = ".ts";
pub const DASH_EXT: &str = ".mpd";
pub const HLS_PREFIX: &str = "hls";
pub const CUSTOM_VIDEO_PREFIX: &str = "cvs";
pub const HLS_EXT_QUERY: &str = ".m3u8?";
pub const HLS_EXT_FRAGMENT: &str = ".m3u8#";
pub const DASH_EXT_QUERY: &str = ".mpd?";
pub const DASH_EXT_FRAGMENT: &str = ".mpd#";
pub const CHANNEL_UNAVAILABLE: &str = "channel_unavailable.ts";
pub const USER_CONNECTIONS_EXHAUSTED: &str = "user_connections_exhausted.ts";
pub const PROVIDER_CONNECTIONS_EXHAUSTED: &str = "provider_connections_exhausted.ts";
pub const LOW_PRIORITY_PREEMPTED: &str = "low_priority_preempted.ts";
pub const USER_ACCOUNT_EXPIRED: &str = "user_account_expired.ts";
pub const PANEL_API_PROVISIONING: &str = "panel_api_provisioning.ts";
pub const HLS_SESSION_OR_LEASE_EXPIRED: &str = "hls_session_or_lease_expired.ts";
pub const PANEL_API_PROVISIONING_HLS_SEGMENT_COUNT: usize = 6;
pub const PANEL_API_PROVISIONING_HLS_SEGMENT_PREFIX: &str = "panel_api_provisioning_hls_";
