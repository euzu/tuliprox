//! EPG defaults: match thresholds, normalize/strip patterns, episode pattern.

default_eq_fns!(
    default_epg_match_threshold, is_default_epg_match_threshold, u16, 80;
    default_epg_best_match_threshold, is_default_epg_best_match_threshold, u16, 95;
);

pub const DEFAULT_EPG_NORMALIZE_REGEX: &str = r"[^a-zA-Z0-9\-]";

pub fn default_epg_normalize_regex() -> Option<String> { Some(DEFAULT_EPG_NORMALIZE_REGEX.to_string()) }
pub fn is_default_epg_normalize_regex(v: &Option<String>) -> bool {
    match v.as_ref().map(|value| value.trim()) {
        None => true,
        Some(value) => value.is_empty() || value == DEFAULT_EPG_NORMALIZE_REGEX,
    }
}

pub const DEFAULT_EPG_STRIP: &[&str] = &["3840p", "uhd", "fhd", "hd", "sd", "4k", "plus", "raw", "full hd"];
pub const DEFAULT_EPG_NAME_PREFIX_SEPARATOR: &[char] = &[':', '|', '-'];

pub fn default_epg_strip() -> Option<Vec<String>> {
    Some(DEFAULT_EPG_STRIP.iter().map(|item| (*item).to_string()).collect())
}
pub fn is_default_epg_strip(v: &Option<Vec<String>>) -> bool {
    let Some(current) = v.as_ref() else {
        return true;
    };
    let Some(default_strip) = default_epg_strip() else {
        return false;
    };
    current == &default_strip
}

pub fn default_epg_name_prefix_separator() -> Option<Vec<char>> { Some(DEFAULT_EPG_NAME_PREFIX_SEPARATOR.to_vec()) }
pub fn is_default_epg_name_prefix_separator(v: &Option<Vec<char>>) -> bool {
    let Some(current) = v.as_ref() else {
        return true;
    };
    let Some(default_separator) = default_epg_name_prefix_separator() else {
        return false;
    };
    current == &default_separator
}

pub const DEFAULT_EPISODE_PATTERN: &str = r".*(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2}).*";

pub fn default_episode_pattern() -> Option<String> { Some(DEFAULT_EPISODE_PATTERN.to_string()) }

pub fn is_blank_or_default_episode_pattern(path: &Option<String>) -> bool {
    path.as_ref().is_none_or(|value| value.trim().is_empty() || value.trim() == DEFAULT_EPISODE_PATTERN)
}
