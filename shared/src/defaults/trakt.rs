//! Trakt defaults.

pub const TRAKT_API_VERSION: &str = "2";
pub const TRAKT_API_URL: &str = "https://api.trakt.tv";

pub fn default_trakt_api_version() -> String { String::from(TRAKT_API_VERSION) }
pub fn default_trakt_api_url() -> String { String::from(TRAKT_API_URL) }
pub fn default_trakt_fuzzy_threshold() -> u8 { 80 }
