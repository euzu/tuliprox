use super::MimeCategory;
use reqwest::StatusCode;
use shared::utils::CONTENT_TYPE_JSON;
use url::Url;

pub fn classify_content_type(headers: &[(String, String)]) -> MimeCategory {
    headers.iter().find_map(|(k, v)| (k == axum::http::header::CONTENT_TYPE.as_str()).then_some(v)).map_or(
        MimeCategory::Unknown,
        |v| match v.to_lowercase().as_str() {
            v if v.starts_with("video/") || v == "application/octet-stream" => MimeCategory::Video,
            v if v.contains("mpegurl") => MimeCategory::M3U8,
            v if v.starts_with("image/") => MimeCategory::Image,
            v if v.starts_with(CONTENT_TYPE_JSON) || v.ends_with("+json") => MimeCategory::Json,
            v if v.starts_with("application/xml") || v.ends_with("+xml") || v == "text/xml" => MimeCategory::Xml,
            v if v.starts_with("text/") => MimeCategory::Text,
            _ => MimeCategory::Unclassified,
        },
    )
}

pub fn format_http_status(status: StatusCode) -> String {
    let code = status.as_u16();
    match status.canonical_reason() {
        Some(reason) => format!("{code} {reason}"),
        None => code.to_string(),
    }
}

pub fn content_type_from_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "fmp4" | "m4s" | "m4v" | "cmfv" => "video/mp4",
        "m4a" | "cmfa" => "audio/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "ts" => "video/mp2t",
        _ => "application/octet-stream",
    }
}

pub fn parse_range(range: &str) -> Option<(u64, Option<u64>)> {
    // expect: "bytes=START-END"
    if !range.starts_with("bytes=") {
        return None;
    }

    let range = &range[6..];
    let mut parts = range.split('-');

    let start = parts.next()?.parse().ok()?;
    let end = parts.next().and_then(|s| s.parse().ok());

    Some((start, end))
}

pub fn is_file_url(url: &str) -> bool { Url::parse(url).is_ok_and(|u| u.scheme().eq_ignore_ascii_case("file")) }

pub fn is_uri(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| {
        u.scheme().eq_ignore_ascii_case("file")
            || u.scheme().eq_ignore_ascii_case("http")
            || u.scheme().eq_ignore_ascii_case("https")
    })
}
