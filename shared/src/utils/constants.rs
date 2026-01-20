use regex::Regex;
use std::collections::HashSet;
use std::string::ToString;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock};


pub const USER_FILE: &str = "user.txt";
pub const CONFIG_PATH: &str = "config";
pub const CONFIG_FILE: &str = "config.yml";
pub const SOURCE_FILE: &str = "source.yml";
pub const MAPPING_FILE: &str = "mapping.yml";
pub const API_PROXY_FILE: &str = "api-proxy.yml";

pub const ENCODING_GZIP: &str = "gzip";
pub const ENCODING_DEFLATE: &str = "deflate";


pub const HLS_EXT: &str = ".m3u8";
pub const DASH_EXT: &str = ".mpd";

pub const HLS_PREFIX: &str = "hls";
pub const CUSTOM_VIDEO_PREFIX: &str = "cvs";

pub const HLS_EXT_QUERY: &str = ".m3u8?";
pub const HLS_EXT_FRAGMENT: &str = ".m3u8#";
pub const DASH_EXT_QUERY: &str = ".mpd?";
pub const DASH_EXT_FRAGMENT: &str = ".mpd#";

pub const FILENAME_TRIM_PATTERNS: &[char] = &['.', '-', '_'];

const SUPPORTED_RESPONSE_HEADERS: &[&str] = &[
    //"accept",
    "accept-ranges",
    "content-type",
    "content-length",
    "content-range",
    "vary",
    "transfer-encoding",
    //"connection",
    "access-control-allow-origin",
    "access-control-allow-credentials",
    "icy-metadata",
    "icy-metaint",
    "referer",
    "last-modified",
    "cache-control",
    "etag",
    "expires"
];


pub fn filter_response_header(key: &str) -> bool {
    SUPPORTED_RESPONSE_HEADERS.contains(&key)
}

pub fn filter_request_header(key: &str) -> bool {
    if key == "host" || key == "connection" {
        return false;
    }
    true
}

/// Configuration for media export naming styles (Kodi, Plex, Emby, Jellyfin)
pub struct ExportStyleConfig {
    pub year: Regex,
    pub season: Regex,
    pub episode: Regex,
    pub whitespace: Regex,
    pub alphanumeric: Regex,
    pub paaren: Regex,
}

pub struct Constants {
    pub re_credentials: Regex,
    pub re_ipv4: Regex,
    pub re_ipv6: Regex,
    pub re_stream_url: Regex,
    pub re_url: Regex,
    pub re_password: Regex,
    pub re_base_href_tag: Regex,
    pub re_base_href: Regex,
    pub re_base_href_wasm: Regex,
    pub re_env_var: Regex,
    pub re_memory_usage: Regex,
    pub re_epg_normalize: Arc<Regex>,
    pub re_template_var: Regex,
    pub re_template_tag: Regex,
    pub re_template_attribute: Regex,
    pub re_filename: Regex,
    pub re_remove_filename_ending: Regex,
    pub re_whitespace: Regex,
    pub re_hls_uri: Regex,
    pub sanitize: AtomicBool,
    pub export_style_config: ExportStyleConfig,
    pub country_codes: HashSet<&'static str>,
    pub allowed_output_formats: Vec<String>,
    pub re_trakt_year: Regex,
    pub re_quality: Regex,
    pub re_classifier_year: Regex,
    pub re_classifier_cleanup: Regex,
    pub re_classifier_episode: Regex,
    pub re_classifier_season: Regex,
    pub re_classifier_moviedb_id: Regex,
    pub re_classifier_camel_case: Regex,
    pub re_classifier_brackets_info: Regex,
}

pub static CONSTANTS: LazyLock<Constants> = LazyLock::new(||
    Constants {
        re_credentials: Regex::new(r"((username|password|token)=)[^&]*").unwrap(),
        re_ipv4: Regex::new(r"\b((25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\b").unwrap(),
        re_ipv6: Regex::new(r"\b([0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}\b|\b::([0-9a-fA-F]{1,4}:){0,6}[0-9a-fA-F]{1,4}\b|\b([0-9a-fA-F]{1,4}:){1,6}:[0-9a-fA-F]{1,4}\b|\b::ffff:(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\b").unwrap(),
        re_stream_url: Regex::new(r"(?i)^(?P<scheme>https?://)[^/]+/(?P<ctx>live|video|movie|series|m3u-stream|resource)/[^/]+/[^/]+/").unwrap(),
        re_url: Regex::new(r"(.*://).*?/(.*)").unwrap(),
        re_password: Regex::new(r"password:\s*(\w+)").unwrap(),
        re_base_href_tag: Regex::new(r"(?is)<base\b[^>]*>").unwrap(),
        re_base_href: Regex::new(r#"(href|src)="/([^"]*)""#).unwrap(),
        re_base_href_wasm: Regex::new("'(/frontend\\-)").unwrap(),
        re_env_var: Regex::new(r"\$\{env:(?P<var>[a-zA-Z_][a-zA-Z0-9_]*)}").unwrap(),
        re_memory_usage: Regex::new(r"VmRSS:\s+(\d+) kB").unwrap(),
        re_epg_normalize: Arc::new(Regex::new(r"[^a-zA-Z0-9\-]").unwrap()),
        re_template_var: Regex::new("!(.*?)!").unwrap(),
        re_template_tag: Regex::new("<tag:(.*?)>").unwrap(),
        re_template_attribute: Regex::new("<(.*?)>").unwrap(),
        re_filename: Regex::new(r"[^A-Za-z0-9_.-]").unwrap(),
        re_remove_filename_ending: Regex::new(r"[_.\s-]$").unwrap(),
        re_whitespace: Regex::new(r"\s+").unwrap(),
        re_hls_uri: Regex::new(r#"URI=["']([^"']+)["']"#).unwrap(),

        sanitize: AtomicBool::new(true),
        export_style_config: ExportStyleConfig {
            season: Regex::new(r"[Ss]\d{1,2}").unwrap(),
            episode: Regex::new(r"[Ee]\d{1,2}").unwrap(),
            year: Regex::new(r"(\d{4})").unwrap(),
            whitespace: Regex::new(r"\s+").unwrap(),
            alphanumeric: Regex::new(r"[^\w\s]").unwrap(),
            paaren: Regex::new(r"(\(\)|\[\]|\{\})").unwrap(),
        },
        allowed_output_formats: Vec::from(["m3u8".to_string(), "ts".to_string()]),
        country_codes: vec![
            "af", "al", "dz", "ad", "ao", "ag", "ar", "am", "au", "at", "az", "bs", "bh", "bd", "bb", "by",
            "be", "bz", "bj", "bt", "bo", "ba", "bw", "br", "bn", "bg", "bf", "bi", "bu", "cv", "kh", "cm",
            "ca", "cf", "td", "cl", "cn", "co", "km", "cg", "cr", "hr", "cu", "cy", "cz", "cd", "dk", "dj",
            "dm", "do", "tl", "ec", "eg", "sv", "gq", "er", "ee", "sz", "et", "fj", "fi", "fr", "ga", "gm",
            "ge", "de", "gh", "gr", "gd", "gt", "gn", "gw", "gy", "ht", "hn", "hu", "is", "in", "id", "ir",
            "iq", "ie", "il", "it", "ci", "jm", "jp", "jo", "kz", "ke", "ki", "kp", "kr", "kw", "kg", "la",
            "lv", "lb", "ls", "lr", "ly", "li", "lt", "lu", "mg", "mw", "my", "mv", "ml", "mt", "mh", "mr",
            "mu", "mx", "fm", "md", "mc", "mn", "me", "ma", "mz", "mm", "na", "nr", "np", "nl", "nz", "ni",
            "ne", "ng", "mk", "no", "om", "pk", "pw", "pa", "pg", "py", "pe", "ph", "pl", "pt", "qa", "ro",
            "ru", "rw", "kn", "lc", "vc", "ws", "sm", "st", "sa", "sn", "rs", "sc", "sl", "sg", "sk", "si",
            "sb", "so", "za", "ss", "es", "lk", "sd", "sr", "se", "ch", "sy", "tw", "tj", "tz", "th", "tg",
            "to", "tt", "tn", "tr", "tm", "tv", "ug", "ua", "ae", "gb", "us", "uy", "uz", "vu", "va", "ve",
            "vn", "ye", "zm", "zw", "sat", "4k",
        ].into_iter().collect::<HashSet<&str>>(),
        re_trakt_year: Regex::new(r"\(?(\d{4})\)?$").unwrap(),
        re_quality: Regex::new(r"(?i)\b(x265|4K|UHD|8K|2160p?|1080p?|720p?|480p?|BLURAY|HDTV|DVDRIP|BRRIP|CAM|TS|HDR|DV|SDR)\b").unwrap(),
        re_classifier_year: Regex::new(r"[\(\[]?(\d{4})[\)\]]?").unwrap(),
        re_classifier_cleanup: Regex::new(r"(?i)[\s\._-]*(?:s\d+e\d+|\d+x\d+|season[\s\._-]*\d+|episode[\s\._-]*\d+).*$").unwrap(),
        re_classifier_episode: Regex::new(r"(?i)(?:e|episode|x)[\s\._-]*(\d+)").unwrap(),
        re_classifier_season: Regex::new(r"(?i)(?:s|season)[\s\._-]*(\d+)").unwrap(),
        re_classifier_moviedb_id: Regex::new(r"(?i)\b(tmdb|tvdb|imdb)[\s._=-]?(\d+)\b").unwrap(),
        re_classifier_camel_case: Regex::new(r"([a-z])([A-Z])").unwrap(),
        re_classifier_brackets_info: Regex::new(r"[\[\{\(].*?[\]\}\)]").unwrap(),
    }
);
