//! TMDB defaults: API key, rate limit, language, cache duration, cooldown.

pub const TMDB_API_KEY: &str = "4219e299c89411838049ab0dab19ebd5";

pub fn default_tmdb_api_key() -> Option<String> { Some(TMDB_API_KEY.to_string()) }
pub fn is_tmdb_default_api_key(s: &Option<String>) -> bool { s.as_ref().is_none_or(|s| s == TMDB_API_KEY) }

pub const DEFAULT_TMDB_RATE_LIMIT_MS: u64 = 250;
pub const DEFAULT_TMDB_CACHE_DURATION_DAYS: u32 = 30;
pub const DEFAULT_TMDB_LANGUAGE: &str = "en-US";

pub const fn default_tmdb_rate_limit_ms() -> u64 { DEFAULT_TMDB_RATE_LIMIT_MS }
pub const fn default_tmdb_cache_duration_days() -> u32 { DEFAULT_TMDB_CACHE_DURATION_DAYS }
pub fn default_tmdb_language() -> String { DEFAULT_TMDB_LANGUAGE.to_owned() }
pub const fn is_default_tmdb_rate_limit_ms(v: &u64) -> bool { *v == DEFAULT_TMDB_RATE_LIMIT_MS }
pub const fn is_default_tmdb_cache_duration_days(v: &u32) -> bool { *v == DEFAULT_TMDB_CACHE_DURATION_DAYS }
pub fn is_default_tmdb_language(v: &String) -> bool { v == DEFAULT_TMDB_LANGUAGE }

pub const fn default_tmdb_match_threshold() -> u16 { 86 }
pub const fn is_default_tmdb_match_threshold(v: &u16) -> bool { *v == default_tmdb_match_threshold() }

pub fn default_metadata_tmdb_cooldown() -> String { "7d".to_string() }
pub fn is_default_metadata_tmdb_cooldown(v: &String) -> bool { *v == default_metadata_tmdb_cooldown() }
