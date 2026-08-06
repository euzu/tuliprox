//! EPG defaults: match thresholds, normalize/strip patterns, episode pattern.

default_eq_fns!(
    default_epg_match_threshold, is_default_epg_match_threshold, u16, 80;
    default_epg_best_match_threshold, is_default_epg_best_match_threshold, u16, 95;
);

default_eq_fns!(
    default_ics_dummy_days_past, is_default_ics_dummy_days_past, u16, 1;
    default_ics_dummy_days_future, is_default_ics_dummy_days_future, u16, 14;
    default_ics_dummy_block_hours, is_default_ics_dummy_block_hours, u8, 4;
    default_ics_dummy_min_gap_minutes, is_default_ics_dummy_min_gap_minutes, u16, 1;
    default_ics_max_events, is_default_ics_max_events, usize, 50_000;
    default_ics_max_download_bytes, is_default_ics_max_download_bytes, u64, 10 * 1024 * 1024;
    default_ics_max_decompressed_bytes, is_default_ics_max_decompressed_bytes, usize, 20 * 1024 * 1024;
);

pub const MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT: u64 = 50 * 1024 * 1024;
pub const MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT: usize = 100 * 1024 * 1024;
pub const MAX_ICS_EVENTS_HARD_LIMIT: usize = 200_000;
pub const MAX_ICS_LINE_LENGTH: usize = 128 * 1024;
pub const MAX_ICS_PROPERTIES_PER_EVENT: usize = 256;
pub const MAX_ICS_SUMMARY_LENGTH: usize = 4 * 1024;
pub const MAX_ICS_DESCRIPTION_LENGTH: usize = 64 * 1024;
pub const MAX_ICS_DAYS_PAST: u16 = 30;
pub const MAX_ICS_DAYS_FUTURE: u16 = 366;

pub const DEFAULT_ICS_TIMEZONE: &str = "UTC";
pub const DEFAULT_ICS_EVENT_TITLE: &str = "{summary}";
pub const DEFAULT_ICS_EVENT_DESCRIPTION: &str = "{description}";
pub const DEFAULT_ICS_DUMMY_TITLE: &str = "No programme entry";

pub fn default_ics_timezone() -> String { DEFAULT_ICS_TIMEZONE.to_string() }
pub fn is_default_ics_timezone(value: &String) -> bool { value == DEFAULT_ICS_TIMEZONE }

pub fn default_ics_event_title() -> String { DEFAULT_ICS_EVENT_TITLE.to_string() }
pub fn is_default_ics_event_title(value: &String) -> bool { value == DEFAULT_ICS_EVENT_TITLE }

pub fn default_ics_event_description() -> String { DEFAULT_ICS_EVENT_DESCRIPTION.to_string() }
pub fn is_default_ics_event_description(value: &String) -> bool { value == DEFAULT_ICS_EVENT_DESCRIPTION }

pub fn default_ics_dummy_title() -> String { DEFAULT_ICS_DUMMY_TITLE.to_string() }
pub fn is_default_ics_dummy_title(value: &String) -> bool { value == DEFAULT_ICS_DUMMY_TITLE }

pub const DEFAULT_EPG_NORMALIZE_REGEX: &str = r"[^a-zA-Z0-9._\-]";

pub fn default_epg_normalize_regex() -> Option<String> { Some(DEFAULT_EPG_NORMALIZE_REGEX.to_string()) }
pub fn is_default_epg_normalize_regex(v: &Option<String>) -> bool {
    match v.as_ref().map(|value| value.trim()) {
        None => true,
        Some(value) => value.is_empty() || value == DEFAULT_EPG_NORMALIZE_REGEX,
    }
}

pub const DEFAULT_EPG_STRIP: &[&str] = &[
    "3840p", "2160p", "1080p", "720p", "576p", "uhd", "fhd", "full hd", "hd", "sd", "4k", "h265", "h264", "hevc",
    "50fps", "60fps", "plus", "raw",
];
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
