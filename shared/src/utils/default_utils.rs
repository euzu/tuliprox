use std::sync::Arc;
use crate::model::{ConfigTargetOptions, LibraryMetadataFormat, ProcessingOrder};

pub const fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}
pub const fn is_true(v: &bool) -> bool {
    *v
}
pub const fn is_false(v: &bool) -> bool {
    !*v
}
pub const fn default_as_true() -> bool {
    true
}

pub fn is_blank_optional_string(s: &Option<String>) -> bool {
    s.as_ref()
        .is_none_or(|s| s.chars().all(|c| c.is_whitespace()))
}

pub fn is_blank_optional_arc_str(s: &Option<Arc<str>>) -> bool {
    s.as_ref().is_none_or(|s| s.chars().all(|c| c.is_whitespace()))
}

pub fn is_empty_optional_vec<T>(s: &Option<Vec<T>>) -> bool {
    s.as_ref().is_none_or(|v| v.is_empty())
}

pub fn default_as_default() -> String {
    "default".into()
}
// Default delay values for resolving VOD or Series requests,
// used to prevent frequent requests that could trigger a provider ban.
pub const fn default_resolve_delay_secs() -> u16 {
    2
}
pub const fn is_default_resolve_delay_secs(v: &u16) -> bool {
    *v == default_resolve_delay_secs()
}
// Default grace values to accommodate rapid channel changes and seek requests,
// helping avoid triggering hard max_connection enforcement.
pub const fn default_grace_period_millis() -> u64 {
    2000
}
pub const fn is_default_grace_period_millis(v: &u64) -> bool {
    *v == default_grace_period_millis()
}
pub const fn default_shared_burst_buffer_mb() -> u64 {
    12
}
pub const fn is_default_shared_burst_buffer_mb(v: &u64) -> bool {
    *v == default_shared_burst_buffer_mb()
}
pub const fn default_grace_period_timeout_secs() -> u64 {
    4
}
pub const fn is_default_grace_period_timeout_secs(v: &u64) -> bool {
    *v == default_grace_period_timeout_secs()
}
pub const fn default_panel_api_provision_timeout_secs() -> u64 {
    65
}
pub const fn default_panel_api_provision_probe_interval_secs() -> u64 {
    15
}
pub const fn default_panel_api_provision_cooldown_secs() -> u64 {
    0
}
pub const fn default_panel_api_alias_pool_min() -> u16 {
    1
}
pub const fn default_panel_api_alias_pool_max() -> u16 {
    1
}
pub const fn default_connect_timeout_secs() -> u32 {
    6
}
pub const fn is_default_connect_timeout_secs(v: &u32) -> bool {
    *v == default_connect_timeout_secs()
}
pub const fn default_resource_retry_attempts() -> u32 {
    3
}
pub const fn is_default_resource_retry_attempts(v: &u32) -> bool {
    *v == default_resource_retry_attempts()
}
pub const fn default_resource_retry_backoff_ms() -> u64 {
    250
}
pub const fn is_default_resource_retry_backoff_ms(v: &u64) -> bool {
    *v == default_resource_retry_backoff_ms()
}
pub const fn default_resource_retry_backoff_multiplier() -> f64 {
    1.0
}
pub const F64_DEFAULT_EPSILON: f64 = 1e-9;
pub const fn is_default_resource_retry_backoff_multiplier(v: &f64) -> bool {
    (*v - default_resource_retry_backoff_multiplier()).abs() < F64_DEFAULT_EPSILON
}

pub fn default_secret() -> String {
    let mut out = [0u8; 16];
    for x in &mut out {
        *x = fastrand::u8(..);
    }
    out.iter().map(|b| format!("{:02X}", b)).collect()
}

pub const fn default_kick_secs() -> u64 {
    90
}
pub const fn is_default_kick_secs(v: &u64) -> bool {
    *v == default_kick_secs()
}
/// 30 minutes by default; `0` still means “no expiration.”
pub const fn default_token_ttl_mins() -> u32 {
    30
}
pub const fn is_default_token_ttl_mins(v: &u32) -> bool {
    *v == default_token_ttl_mins()
}

pub const fn default_match_threshold() -> u16 {
    80
}
pub const fn is_default_match_threshold(v: &u16) -> bool {
    *v == default_match_threshold()
}
pub const fn default_best_match_threshold() -> u16 {
    95
}
pub const fn is_default_best_match_threshold(v: &u16) -> bool {
    *v == default_best_match_threshold()
}

pub const TMDB_API_KEY: &str = "4219e299c89411838049ab0dab19ebd5";
pub fn default_tmdb_api_key() -> Option<String> {
    Some(TMDB_API_KEY.to_string())
}
pub fn is_tmdb_default_api_key(s: &Option<String>) -> bool {
    s.as_ref().is_none_or(|s| s == TMDB_API_KEY)
}
pub fn is_default_tmdb_language(v: &String) -> bool {
    v == DEFAULT_TMDB_LANGUAGE
}

pub fn default_metadata_path() -> String {
    "library_metadata".to_string()
}

pub const DEFAULT_TMDB_RATE_LIMIT_MS: u64 = 250;
pub const DEFAULT_TMDB_CACHE_DURATION_DAYS: u32 = 30;
pub const DEFAULT_TMDB_LANGUAGE: &str = "en-US";
pub const fn default_tmdb_rate_limit_ms() -> u64 {
    DEFAULT_TMDB_RATE_LIMIT_MS
}
pub const fn default_tmdb_cache_duration_days() -> u32 {
    DEFAULT_TMDB_CACHE_DURATION_DAYS
}
pub fn default_tmdb_language() -> String {
    DEFAULT_TMDB_LANGUAGE.to_owned()
}
pub fn is_default_tmdb_rate_limit_ms(v: &u64) -> bool {
    *v == DEFAULT_TMDB_RATE_LIMIT_MS
}
pub fn is_default_tmdb_cache_duration_days(v: &u32) -> bool {
    *v == DEFAULT_TMDB_CACHE_DURATION_DAYS
}

pub fn default_storage_formats() -> Vec<LibraryMetadataFormat> {
    vec![]
}
pub fn default_movie_category() -> String {
    String::from("Local Movies")
}
pub fn default_series_category() -> String {
    String::from("Local TV Shows")
}

pub const DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS: &[&str] =
    &["mp4", "mkv", "avi", "mov", "ts", "m4v", "webm"];

pub fn default_supported_library_extensions() -> Vec<String> {
    DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

pub fn is_default_supported_library_extensions(v: &[String]) -> bool {
    v.len() == DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS.len()
        && v.iter()
            .zip(DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS)
            .all(|(a, b)| a == b)
}

pub const DEFAULT_VIDEO_EXTENSIONS: &[&str] = &["mkv", "avi", "mp4", "mpeg", "divx", "mov"];

pub fn default_supported_video_extensions() -> Vec<String> {
    DEFAULT_VIDEO_EXTENSIONS
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

pub fn is_default_supported_video_extensions(v: &[String]) -> bool {
    v.len() == DEFAULT_VIDEO_EXTENSIONS.len()
        && v.iter().zip(DEFAULT_VIDEO_EXTENSIONS).all(|(a, b)| a == b)
}

pub fn is_config_target_options_empty(v: &Option<ConfigTargetOptions>) -> bool {
    v.as_ref().is_none_or(|c| c.is_empty())
}

pub fn is_default_processing_order(p: &ProcessingOrder) -> bool {
    *p == ProcessingOrder::default()
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
pub fn default_friendly_name() -> String {
    DEFAULT_FRIENDLY_NAME.into()
}
pub fn default_manufacturer() -> String {
    DEFAULT_MANUFACTURER.into()
}
pub fn default_model_name() -> String {
    DEFAULT_MODEL_NAME.into()
}
pub fn default_firmware_name() -> String {
    DEFAULT_FIRMWARE_NAME.into()
}
pub fn default_firmware_version() -> String {
    DEFAULT_FIRMWARE_VERSION.into()
}
pub fn default_device_type() -> String {
    DEFAULT_DEVICE_TYPE.into()
}
pub fn default_device_udn() -> String {
    DEFAULT_DEVICE_UDN.into()
}
pub fn is_default_friendly_name(value: &String) -> bool {
    value == DEFAULT_FRIENDLY_NAME
}
pub fn is_default_manufacturer(value: &String) -> bool {
    value == DEFAULT_MANUFACTURER
}
pub fn is_default_model_name(value: &String) -> bool {
    value == DEFAULT_MODEL_NAME
}
pub fn is_default_firmware_name(value: &String) -> bool {
    value == DEFAULT_FIRMWARE_NAME
}
pub fn is_default_firmware_version(value: &String) -> bool {
    value == DEFAULT_FIRMWARE_VERSION
}
pub fn is_default_device_type(value: &String) -> bool {
    value == DEFAULT_DEVICE_TYPE
}
pub fn is_default_device_udn(value: &String) -> bool {
    value == DEFAULT_DEVICE_UDN
}

//////////////////////////
// trakt
////////////////////////////
pub const TRAKT_API_KEY: &str = "0183a05ad97098d87287fe46da4ae286f434f32e8e951caad4cc147c947d79a3";
pub const TRAKT_API_VERSION: &str = "2";
pub const TRAKT_API_URL: &str = "https://api.trakt.tv";

pub fn default_trakt_api_key() -> String {
    String::from(TRAKT_API_KEY)
}

pub fn default_trakt_api_version() -> String {
    String::from(TRAKT_API_VERSION)
}

pub fn default_trakt_api_url() -> String {
    String::from(TRAKT_API_URL)
}

pub fn default_trakt_fuzzy_threshold() -> u8 {
    80
}
