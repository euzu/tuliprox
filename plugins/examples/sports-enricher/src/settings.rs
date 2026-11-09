//! Defaults and accessors for the plugin settings (`config-schema.json`).
//!
//! The struct itself lives in `lib.rs`, inside the `#[tuliprox_plugin]` module,
//! because the generated default `lifecycle` parses it at `init`.

use crate::Settings;

pub const DEFAULT_API_BASE_URL: &str = "http://127.0.0.1:18970";

/// The leagues tracked when the `leagues` setting is empty.
pub const DEFAULT_LEAGUES: [&str; 3] = ["epl", "laliga", "nba"];

/// Most leagues one configuration may track (bounds the API calls of a refresh).
pub const MAX_LEAGUES: usize = 12;

pub fn default_api_base_url() -> String { DEFAULT_API_BASE_URL.to_string() }

pub const fn default_group_emoji() -> bool { true }

impl Settings {
    /// The API key, or `None` when it is unset or blank.
    #[must_use]
    pub fn api_key_value(&self) -> Option<&str> {
        self.api_key.as_ref().map(|secret| secret.expose().trim()).filter(|key| !key.is_empty())
    }

    /// The API base URL without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str { self.api_base_url.trim().trim_end_matches('/') }

    /// The leagues to track, in the configured order: lowercase, unique, only
    /// `[a-z0-9-]` (they go into a URL and a KV key). Empty means the defaults.
    #[must_use]
    pub fn league_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for raw in &self.leagues {
            let id = raw.trim().to_lowercase();
            let valid = !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            if valid && !ids.contains(&id) {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            ids = DEFAULT_LEAGUES.iter().map(|id| (*id).to_string()).collect();
        }
        ids.truncate(MAX_LEAGUES);
        ids
    }
}

#[cfg(test)]
mod tests {
    use crate::Settings;
    use tuliprox_plugin_sdk::prelude::PluginSettings;

    #[test]
    fn defaults_and_accessors() {
        let settings = Settings::parse("{}").unwrap();
        assert_eq!(settings.base_url(), "http://127.0.0.1:18970");
        assert!(settings.group_emoji);
        assert_eq!(settings.api_key_value(), None);
        assert_eq!(settings.league_ids(), ["epl", "laliga", "nba"]);

        let settings = Settings::parse(
            r#"{"api_key":" k ","api_base_url":"http://x:1/","leagues":["NBA","nba","bad id","epl"],"group_emoji":false}"#,
        )
        .unwrap();
        assert_eq!(settings.api_key_value(), Some("k"));
        assert_eq!(settings.base_url(), "http://x:1");
        assert_eq!(settings.league_ids(), ["nba", "epl"]);
        assert!(!settings.group_emoji);
        assert_eq!(Settings::parse(r#"{"api_key":""}"#).unwrap().api_key_value(), None);
    }
}
