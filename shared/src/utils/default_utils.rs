use crate::{
    model::{ConfigTargetOptions, LibraryMetadataFormat, MetadataUpdateConfigDto, ProcessingOrder, VideoConfigDto},
    utils::{
        CONFIG_PATH, DEFAULT_BACKUP_DIR, DEFAULT_CACHE_DIR, DEFAULT_CUSTOM_STREAM_RESPONSE_PATH, DEFAULT_DOWNLOAD_DIR,
        DEFAULT_EPISODE_PATTERN, DEFAULT_STORAGE_DIR, DEFAULT_USER_AGENT, DEFAULT_USER_CONFIG_DIR, DEFAULT_WEB_DIR,
        MAPPING_FILE, TEMPLATE_FILE, USER_FILE,
    },
};
use std::sync::Arc;

pub const fn is_zero_u16(v: &u16) -> bool { *v == 0 }
pub const fn is_zero_i16(v: &i16) -> bool { *v == 0 }
pub const fn is_zero_u32(v: &u32) -> bool { *v == 0 }
pub const fn is_true(v: &bool) -> bool { *v }
pub const fn is_false(v: &bool) -> bool { !*v }
pub const fn default_as_true() -> bool { true }

pub fn is_blank_optional_string(s: &Option<String>) -> bool {
    s.as_ref().is_none_or(|s| s.chars().all(|c| c.is_whitespace()))
}

pub fn is_blank_optional_arc_str(s: &Option<Arc<str>>) -> bool {
    s.as_ref().is_none_or(|s| s.chars().all(|c| c.is_whitespace()))
}

pub fn is_empty_optional_vec<T>(s: &Option<Vec<T>>) -> bool { s.as_ref().is_none_or(|v| v.is_empty()) }

pub fn default_as_default() -> String { "default".into() }
// Default delay values for resolving VOD or Series requests,
// used to prevent frequent requests that could trigger a provider ban.
pub const fn default_resolve_delay_secs() -> u16 { 2 }
pub const fn is_default_resolve_delay_secs(v: &u16) -> bool { *v == default_resolve_delay_secs() }
// Default delay values for probing streams (ffprobe),
// used to avoid excessive probing under rapid playlist changes.
pub const fn default_probe_delay_secs() -> u16 { 2 }
pub const fn is_default_probe_delay_secs(v: &u16) -> bool { *v == default_probe_delay_secs() }
// Default grace values to accommodate rapid channel changes and seek requests,
// helping avoid triggering hard max_connection enforcement.
pub const fn default_grace_period_millis() -> u64 { 2000 }
pub const fn is_default_grace_period_millis(v: &u64) -> bool { *v == default_grace_period_millis() }
pub const fn default_shared_burst_buffer_mb() -> u64 { 12 }
pub const fn is_default_shared_burst_buffer_mb(v: &u64) -> bool { *v == default_shared_burst_buffer_mb() }
pub const fn default_grace_period_timeout_secs() -> u64 { 4 }
pub const fn is_default_grace_period_timeout_secs(v: &u64) -> bool { *v == default_grace_period_timeout_secs() }
pub const fn default_hls_session_ttl_secs() -> u64 { 15 }
pub const fn is_default_hls_session_ttl_secs(v: &u64) -> bool { *v == default_hls_session_ttl_secs() }
pub const fn default_catchup_session_ttl_secs() -> u64 { 45 }
pub const fn is_default_catchup_session_ttl_secs(v: &u64) -> bool { *v == default_catchup_session_ttl_secs() }
pub const fn default_panel_api_provision_timeout_secs() -> u64 { 65 }
pub const fn default_panel_api_provision_probe_interval_secs() -> u64 { 15 }
pub const fn default_panel_api_provision_cooldown_secs() -> u64 { 0 }
pub const fn default_panel_api_alias_pool_min() -> u16 { 1 }
pub const fn default_panel_api_alias_pool_max() -> u16 { 1 }
pub const fn default_connect_timeout_secs() -> u32 { 6 }
pub const fn is_default_connect_timeout_secs(v: &u32) -> bool { *v == default_connect_timeout_secs() }
pub const fn default_resource_retry_attempts() -> u32 { 3 }
pub const fn is_default_resource_retry_attempts(v: &u32) -> bool { *v == default_resource_retry_attempts() }
pub const fn default_resource_retry_backoff_ms() -> u64 { 250 }
pub const fn is_default_resource_retry_backoff_ms(v: &u64) -> bool { *v == default_resource_retry_backoff_ms() }
pub const fn default_resource_retry_backoff_multiplier() -> f64 { 1.0 }
pub const F64_DEFAULT_EPSILON: f64 = 1e-9;
pub const fn is_default_resource_retry_backoff_multiplier(v: &f64) -> bool {
    (*v - default_resource_retry_backoff_multiplier()).abs() < F64_DEFAULT_EPSILON
}

fn fill_with_secure_random_bytes(out: &mut [u8]) {
    #[cfg(target_arch = "wasm32")]
    {
        for byte in out {
            *byte = fastrand::u8(..);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    if let Err(err) = getrandom::fill(out) {
        panic!("failed to generate secure random bytes: {err}");
    }
}

pub fn generate_default_access_secret() -> [u8; 32] {
    let mut out = [0u8; 32];
    fill_with_secure_random_bytes(&mut out);
    out
}

pub fn generate_default_encrypt_secret() -> [u8; 16] {
    let mut out = [0u8; 16];
    fill_with_secure_random_bytes(&mut out);
    out
}

pub fn default_secret() -> String { generate_default_encrypt_secret().iter().map(|b| format!("{:02X}", b)).collect() }

pub const fn default_kick_secs() -> u64 { 90 }
pub const fn is_default_kick_secs(v: &u64) -> bool { *v == default_kick_secs() }
/// 30 minutes by default; `0` still means “no expiration.”
pub const fn default_token_ttl_mins() -> u32 { 30 }
pub const fn is_default_token_ttl_mins(v: &u32) -> bool { *v == default_token_ttl_mins() }

pub const fn default_epg_match_threshold() -> u16 { 80 }
pub const fn is_default_epg_match_threshold(v: &u16) -> bool { *v == default_epg_match_threshold() }
pub const fn default_epg_best_match_threshold() -> u16 { 95 }
pub const fn is_default_epg_best_match_threshold(v: &u16) -> bool { *v == default_epg_best_match_threshold() }

pub const fn default_tmdb_match_threshold() -> u16 { 86 }
pub const fn is_default_tmdb_match_threshold(v: &u16) -> bool { *v == default_tmdb_match_threshold() }

pub const TMDB_API_KEY: &str = "4219e299c89411838049ab0dab19ebd5";
pub fn default_tmdb_api_key() -> Option<String> { Some(TMDB_API_KEY.to_string()) }
pub fn is_tmdb_default_api_key(s: &Option<String>) -> bool { s.as_ref().is_none_or(|s| s == TMDB_API_KEY) }
pub fn is_default_tmdb_language(v: &String) -> bool { v == DEFAULT_TMDB_LANGUAGE }

pub const DEFAULT_METADATA_PATH: &str = "metadata";
pub fn default_metadata_path() -> String { DEFAULT_METADATA_PATH.to_string() }
pub fn is_default_metadata_path(s: &str) -> bool { s == DEFAULT_METADATA_PATH }

pub const DEFAULT_TMDB_RATE_LIMIT_MS: u64 = 250;
pub const DEFAULT_TMDB_CACHE_DURATION_DAYS: u32 = 30;
pub const DEFAULT_TMDB_LANGUAGE: &str = "en-US";
pub const fn default_tmdb_rate_limit_ms() -> u64 { DEFAULT_TMDB_RATE_LIMIT_MS }
pub const fn default_tmdb_cache_duration_days() -> u32 { DEFAULT_TMDB_CACHE_DURATION_DAYS }
pub fn default_tmdb_language() -> String { DEFAULT_TMDB_LANGUAGE.to_owned() }
pub fn is_default_tmdb_rate_limit_ms(v: &u64) -> bool { *v == DEFAULT_TMDB_RATE_LIMIT_MS }
pub fn is_default_tmdb_cache_duration_days(v: &u32) -> bool { *v == DEFAULT_TMDB_CACHE_DURATION_DAYS }

pub fn default_storage_formats() -> Vec<LibraryMetadataFormat> { vec![] }
pub fn default_movie_category() -> String { String::from("Local Movies") }
pub fn default_series_category() -> String { String::from("Local TV Shows") }

pub const DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS: &[&str] = &["mp4", "mkv", "avi", "mov", "ts", "m4v", "webm"];

pub fn default_supported_library_extensions() -> Vec<String> {
    DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect()
}

pub fn is_default_supported_library_extensions(v: &[String]) -> bool {
    v.len() == DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS.len()
        && v.iter().zip(DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS).all(|(a, b)| a == b)
}

pub const DEFAULT_VIDEO_EXTENSIONS: &[&str] = &["mkv", "avi", "mp4", "mpeg", "divx", "mov"];

pub fn default_supported_video_extensions() -> Vec<String> {
    DEFAULT_VIDEO_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect()
}

pub fn is_default_supported_video_extensions(v: &[String]) -> bool {
    v.len() == DEFAULT_VIDEO_EXTENSIONS.len() && v.iter().zip(DEFAULT_VIDEO_EXTENSIONS).all(|(a, b)| a == b)
}

pub fn is_config_target_options_empty(v: &Option<ConfigTargetOptions>) -> bool {
    v.as_ref().is_none_or(|c| c.is_empty())
}

pub fn is_default_processing_order(p: &ProcessingOrder) -> bool { *p == ProcessingOrder::default() }

pub const fn default_probe_live_interval() -> u32 { 120 }

pub const fn is_default_probe_live_interval(v: &u32) -> bool { *v == default_probe_live_interval() }

pub const fn default_resolve_background() -> bool { true }
pub const fn default_xtream_live_stream_use_prefix() -> bool { true }

pub fn default_metadata_queue_log_interval() -> String { "30s".to_string() }
pub fn is_default_metadata_queue_log_interval(v: &String) -> bool { *v == default_metadata_queue_log_interval() }
pub fn default_metadata_progress_log_interval() -> String { "15s".to_string() }
pub fn is_default_metadata_progress_log_interval(v: &String) -> bool { *v == default_metadata_progress_log_interval() }
pub fn default_metadata_max_resolve_retry_backoff() -> String { "1h".to_string() }
pub fn is_default_metadata_max_resolve_retry_backoff(v: &String) -> bool {
    *v == default_metadata_max_resolve_retry_backoff()
}
pub fn default_metadata_resolve_min_retry_base() -> String { "5s".to_string() }
pub fn is_default_metadata_resolve_min_retry_base(v: &String) -> bool {
    *v == default_metadata_resolve_min_retry_base()
}
pub fn default_metadata_resolve_exhaustion_reset_gap() -> String { "1h".to_string() }
pub fn is_default_metadata_resolve_exhaustion_reset_gap(v: &String) -> bool {
    *v == default_metadata_resolve_exhaustion_reset_gap()
}
pub fn default_metadata_probe_cooldown() -> String { "7d".to_string() }
pub fn is_default_metadata_probe_cooldown(v: &String) -> bool { *v == default_metadata_probe_cooldown() }
pub fn default_metadata_tmdb_cooldown() -> String { "7d".to_string() }
pub fn is_default_metadata_tmdb_cooldown(v: &String) -> bool { *v == default_metadata_tmdb_cooldown() }
pub fn default_metadata_retry_delay() -> String { "2s".to_string() }
pub fn is_default_metadata_retry_delay(v: &String) -> bool { *v == default_metadata_retry_delay() }
pub fn default_metadata_probe_retry_load_retry_delay() -> String { "1m".to_string() }
pub fn is_default_metadata_probe_retry_load_retry_delay(v: &String) -> bool {
    *v == default_metadata_probe_retry_load_retry_delay()
}
pub fn default_metadata_worker_idle_timeout() -> String { "1m".to_string() }
pub fn is_default_metadata_worker_idle_timeout(v: &String) -> bool { *v == default_metadata_worker_idle_timeout() }
pub fn default_metadata_probe_retry_backoff_step_1() -> String { "10m".to_string() }
pub fn is_default_metadata_probe_retry_backoff_step_1(v: &String) -> bool {
    *v == default_metadata_probe_retry_backoff_step_1()
}
pub fn default_metadata_probe_retry_backoff_step_2() -> String { "30m".to_string() }
pub fn is_default_metadata_probe_retry_backoff_step_2(v: &String) -> bool {
    *v == default_metadata_probe_retry_backoff_step_2()
}
pub fn default_metadata_probe_retry_backoff_step_3() -> String { "1h".to_string() }
pub fn is_default_metadata_probe_retry_backoff_step_3(v: &String) -> bool {
    *v == default_metadata_probe_retry_backoff_step_3()
}
pub const fn default_metadata_max_attempts_resolve() -> u8 { 3 }
pub const fn is_default_metadata_max_attempts_resolve(v: &u8) -> bool { *v == default_metadata_max_attempts_resolve() }
pub const fn default_metadata_max_attempts_probe() -> u8 { 3 }
pub const fn is_default_metadata_max_attempts_probe(v: &u8) -> bool { *v == default_metadata_max_attempts_probe() }
pub const fn default_metadata_backoff_jitter_percent() -> u8 { 20 }
pub const fn is_default_metadata_backoff_jitter_percent(v: &u8) -> bool {
    *v == default_metadata_backoff_jitter_percent()
}
pub const fn default_metadata_max_queue_size() -> usize { 100_000 }
pub const fn is_default_metadata_max_queue_size(v: &usize) -> bool { *v == default_metadata_max_queue_size() }
pub const fn default_metadata_no_change_cache_ttl_secs() -> u64 { 3600 }
pub const fn is_default_metadata_no_change_cache_ttl_secs(v: &u64) -> bool {
    *v == default_metadata_no_change_cache_ttl_secs()
}
pub const fn default_metadata_probe_fairness_resolve_burst() -> usize { 200 }
pub const fn is_default_metadata_probe_fairness_resolve_burst(v: &usize) -> bool {
    *v == default_metadata_probe_fairness_resolve_burst()
}
pub fn default_metadata_ffprobe_analyze_duration() -> String { "10s".to_string() }
pub fn is_default_metadata_ffprobe_analyze_duration(v: &String) -> bool {
    *v == default_metadata_ffprobe_analyze_duration()
}
pub fn default_metadata_ffprobe_probe_size() -> String { "10MB".to_string() }
pub fn is_default_metadata_ffprobe_probe_size(v: &String) -> bool { *v == default_metadata_ffprobe_probe_size() }
pub fn default_metadata_ffprobe_live_analyze_duration() -> String { "5s".to_string() }
pub fn is_default_metadata_ffprobe_live_analyze_duration(v: &String) -> bool {
    *v == default_metadata_ffprobe_live_analyze_duration()
}
pub fn default_metadata_ffprobe_live_probe_size() -> String { "5MB".to_string() }
pub fn is_default_metadata_ffprobe_live_probe_size(v: &String) -> bool {
    *v == default_metadata_ffprobe_live_probe_size()
}
pub fn default_probe_user_priority() -> i8 { 127 }
pub fn is_default_probe_user_priority(v: &i8) -> bool { *v == default_probe_user_priority() }
pub fn default_user_priority() -> i8 { 0 }
pub fn is_default_user_priority(v: &i8) -> bool { *v == default_user_priority() }

pub fn get_default_web_root() -> String { DEFAULT_WEB_DIR.to_string() }
pub fn is_blank_or_default_web_root(value: &str) -> bool {
    let normalized = value.trim().replace('\\', "/");
    if normalized.is_empty() {
        return true;
    }

    let normalized = normalized.trim_end_matches('/');
    normalized.trim_start_matches("./") == DEFAULT_WEB_DIR
}

pub fn is_default_dir_path(value: &str, default_dir: &str) -> bool {
    let normalized = value.trim().replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    let normalized = normalized.trim_start_matches("./");
    let default_dir = default_dir.trim().replace('\\', "/");
    let default_dir = default_dir.trim_end_matches('/');
    normalized == default_dir
}

pub fn is_blank_or_default_download_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_DOWNLOAD_DIR))
}
pub fn default_download_dir() -> Option<String> { Some(DEFAULT_DOWNLOAD_DIR.to_string()) }

pub fn default_episode_pattern() -> Option<String> { Some(DEFAULT_EPISODE_PATTERN.to_string()) }

pub fn is_blank_or_default_episode_pattern(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || value.trim() == DEFAULT_EPISODE_PATTERN)
}

pub fn is_blank_or_default_cache_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_CACHE_DIR))
}

pub fn default_default_user_agent() -> Option<String> { Some(DEFAULT_USER_AGENT.to_string()) }
pub fn default_main_storage_dir() -> Option<String> { Some(DEFAULT_STORAGE_DIR.to_string()) }
pub fn default_main_backup_dir() -> Option<String> { Some(DEFAULT_BACKUP_DIR.to_string()) }
pub fn default_main_user_config_dir() -> Option<String> { Some(DEFAULT_USER_CONFIG_DIR.to_string()) }
pub fn default_main_mapping_path() -> Option<String> { Some(format!("./{CONFIG_PATH}/{MAPPING_FILE}")) }
pub fn default_main_template_path() -> Option<String> { Some(format!("./{CONFIG_PATH}/{TEMPLATE_FILE}")) }
pub fn default_custom_stream_response_path() -> Option<String> { Some(DEFAULT_CUSTOM_STREAM_RESPONSE_PATH.to_string()) }
pub fn default_user_file_path() -> Option<String> { Some(format!("./{CONFIG_PATH}/{USER_FILE}")) }
fn is_default_config_file_path(value: &str, file_name: &str) -> bool {
    let normalized = value.trim().replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    normalized == file_name
        || normalized.rsplit_once('/').is_some_and(|(dir, file)| dir == CONFIG_PATH && file == file_name)
}

pub fn is_blank_or_default_custom_stream_response_path(path: &Option<String>) -> bool {
    path.as_ref()
        .is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_CUSTOM_STREAM_RESPONSE_PATH))
}

pub fn is_blank_or_default_mapping_path(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_config_file_path(value, MAPPING_FILE))
}

pub fn is_blank_or_default_template_path(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_config_file_path(value, TEMPLATE_FILE))
}

pub fn is_blank_or_default_storage_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_STORAGE_DIR))
}

pub fn is_blank_or_default_backup_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_BACKUP_DIR))
}

pub fn is_blank_or_default_user_config_dir(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_dir_path(value, DEFAULT_USER_CONFIG_DIR))
}

pub fn is_blank_or_default_user_file_path(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || is_default_config_file_path(value, USER_FILE))
}

pub fn normalize_optional_dir(path: &Option<String>, default_dir: &str) -> Option<String> {
    path.as_ref().and_then(|value| {
        if value.trim().is_empty() || is_default_dir_path(value, default_dir) {
            None
        } else {
            Some(value.clone())
        }
    })
}

pub fn normalize_optional_config_file_path(path: &Option<String>, default_file_name: &str) -> Option<String> {
    path.as_ref().and_then(|value| {
        if value.trim().is_empty() || is_default_config_file_path(value, default_file_name) {
            None
        } else {
            Some(value.clone())
        }
    })
}

pub fn is_none_or_empty_video(video: &Option<VideoConfigDto>) -> bool {
    video.as_ref().is_none_or(VideoConfigDto::is_empty)
}
pub fn is_none_or_empty_metadata_update(metadata_update: &Option<MetadataUpdateConfigDto>) -> bool {
    metadata_update.as_ref().is_none_or(MetadataUpdateConfigDto::is_empty)
}

//////////////////////////////
// HDHomerun Device Defaults
//////////////////////////////
const DEFAULT_FRIENDLY_NAME: &str = "TuliproxTV";
const DEFAULT_MANUFACTURER: &str = "Silicondust";
const DEFAULT_MODEL_NAME: &str = "HDTC-2US";
const DEFAULT_FIRMWARE_NAME: &str = "hdhomeruntc_atsc";
const DEFAULT_FIRMWARE_VERSION: &str = "20170930";
const DEFAULT_DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const DEFAULT_DEVICE_UDN: &str =
    "uuid:12345678-90ab-cdef-1234-567890abcdef::urn:dial-multicast:com.silicondust.hdhomerun";
pub fn default_friendly_name() -> String { DEFAULT_FRIENDLY_NAME.into() }
pub fn default_manufacturer() -> String { DEFAULT_MANUFACTURER.into() }
pub fn default_model_name() -> String { DEFAULT_MODEL_NAME.into() }
pub fn default_firmware_name() -> String { DEFAULT_FIRMWARE_NAME.into() }
pub fn default_firmware_version() -> String { DEFAULT_FIRMWARE_VERSION.into() }
pub fn default_device_type() -> String { DEFAULT_DEVICE_TYPE.into() }
pub fn default_device_udn() -> String { DEFAULT_DEVICE_UDN.into() }
pub fn is_default_friendly_name(value: &String) -> bool { value == DEFAULT_FRIENDLY_NAME }
pub fn is_default_manufacturer(value: &String) -> bool { value == DEFAULT_MANUFACTURER }
pub fn is_default_model_name(value: &String) -> bool { value == DEFAULT_MODEL_NAME }
pub fn is_default_firmware_name(value: &String) -> bool { value == DEFAULT_FIRMWARE_NAME }
pub fn is_default_firmware_version(value: &String) -> bool { value == DEFAULT_FIRMWARE_VERSION }
pub fn is_default_device_type(value: &String) -> bool { value == DEFAULT_DEVICE_TYPE }
pub fn is_default_device_udn(value: &String) -> bool { value == DEFAULT_DEVICE_UDN }

//////////////////////////
// trakt
////////////////////////////
pub const TRAKT_API_KEY: &str = "0183a05ad97098d87287fe46da4ae286f434f32e8e951caad4cc147c947d79a3";
pub const TRAKT_API_VERSION: &str = "2";
pub const TRAKT_API_URL: &str = "https://api.trakt.tv";

pub fn default_trakt_api_key() -> String { String::from(TRAKT_API_KEY) }

pub fn default_trakt_api_version() -> String { String::from(TRAKT_API_VERSION) }

pub fn default_trakt_api_url() -> String { String::from(TRAKT_API_URL) }

pub fn default_trakt_fuzzy_threshold() -> u8 { 80 }
