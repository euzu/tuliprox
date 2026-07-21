use crate::utils::{encode_m3u_catchup_token, M3uCatchupToken};
use chrono::{DateTime, NaiveDateTime};
use shared::{
    error::TuliproxError,
    model::CatchupProperties,
    utils::{short_hash, Internable},
};
use std::{borrow::Cow, sync::Arc};
use url::Url;

pub const M3U_CATCHUP_ROUTE_PREFIX: &str = "m3u-catchup";
pub const M3U_CATCHUP_MARKER: &str = "tuliprox-catchup";
const COLLECTOR_PREFIX: &str = "v";
const SIPTV_APPEND_TEMPLATE: &str = "?utc={utc}&lutc={lutc}";
const XC_START_TEMPLATE: &str = "{Y}-{m}-{d}:{H}-{M}-{S}";
const FLUSSONIC_TEMPLATE: &str = "timeshift_abs-{utc}";
const XTREAM_BRIDGE_START_FORMATS: [&str; 4] = [
    "%Y-%m-%d %H:%M",
    "%Y-%m-%d:%H-%M",
    "%Y-%m-%d:%H:%M",
    "%Y-%m-%d-%H-%M",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M3uCatchupRewrite {
    pub mode: Arc<str>,
    pub source: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedM3uCatchup {
    pub url: String,
    pub discriminator: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateSegment {
    Literal(String),
    Placeholder(String),
}

fn mode_alias(mode: &str) -> &str {
    if mode.eq_ignore_ascii_case("append") {
        "append"
    } else if mode.eq_ignore_ascii_case("default") {
        "default"
    } else if mode.eq_ignore_ascii_case("shift") {
        "shift"
    } else if mode.eq_ignore_ascii_case("xc") {
        "xc"
    } else if mode.eq_ignore_ascii_case("fs") {
        "fs"
    } else if mode.eq_ignore_ascii_case("vod") {
        "vod"
    } else {
        mode
    }
}

fn parse_template(template: &str) -> Vec<TemplateSegment> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let bytes = template.as_bytes();
    let mut idx = 0usize;

    while idx < bytes.len() {
        let start = if bytes[idx] == b'{' {
            Some((idx, 1usize))
        } else if idx + 1 < bytes.len() && bytes[idx] == b'$' && bytes[idx + 1] == b'{' {
            Some((idx, 2usize))
        } else {
            None
        };

        let Some((placeholder_start, open_len)) = start else {
            current.push(bytes[idx] as char);
            idx += 1;
            continue;
        };

        if !current.is_empty() {
            segments.push(TemplateSegment::Literal(std::mem::take(&mut current)));
        }

        let mut end = placeholder_start + open_len;
        while end < bytes.len() && bytes[end] != b'}' {
            end += 1;
        }
        if end >= bytes.len() {
            current.push_str(&template[placeholder_start..]);
            break;
        }

        segments.push(TemplateSegment::Placeholder(template[placeholder_start..=end].to_string()));
        idx = end + 1;
    }

    if !current.is_empty() {
        segments.push(TemplateSegment::Literal(current));
    }

    segments
}

fn collect_placeholders(segments: &[TemplateSegment]) -> Vec<&str> {
    segments
        .iter()
        .filter_map(|segment| match segment {
            TemplateSegment::Placeholder(value) => Some(value.as_str()),
            TemplateSegment::Literal(_) => None,
        })
        .collect()
}

fn build_local_source(
    base_url: &str,
    path: &str,
    token: &str,
    placeholders: &[&str],
    append_mode: bool,
) -> String {
    let mut source = if append_mode {
        format!("?{M3U_CATCHUP_MARKER}={token}")
    } else {
        format!("{base_url}/{M3U_CATCHUP_ROUTE_PREFIX}/{token}")
    };
    if !append_mode && !path.is_empty() {
        source.push('/');
        source.push_str(path.trim_start_matches('/'));
    }

    for (idx, placeholder) in placeholders.iter().enumerate() {
        let separator = if idx == 0 && !source.contains('?') { '?' } else { '&' };
        source.push(separator);
        source.push_str(COLLECTOR_PREFIX);
        source.push_str(&idx.to_string());
        source.push('=');
        source.push_str(placeholder);
    }

    source
}

fn append_siptv_template(source_url: &str) -> String {
    if source_url.contains('?') {
        format!("&{}", SIPTV_APPEND_TEMPLATE.trim_start_matches('?'))
    } else {
        SIPTV_APPEND_TEMPLATE.to_string()
    }
}

fn derive_xc_template(source_url: &str) -> Option<String> {
    let parsed = Url::parse(source_url).ok()?;
    let mut segments = parsed
        .path_segments()
        .map(|it| it.map(ToString::to_string).collect::<Vec<_>>())?;
    if segments.len() < 3 {
        return None;
    }

    let file_name = segments.pop()?;
    let password = segments.pop()?;
    let username = segments.pop()?;
    if segments.last().is_some_and(|last| last == "live") {
        let _ = segments.pop();
    }
    segments.push("timeshift".to_string());
    segments.push(username);
    segments.push(password);
    segments.push("{duration:60}".to_string());
    segments.push(XC_START_TEMPLATE.to_string());
    segments.push(file_name);

    let mut rebuilt = parsed;
    let new_path = format!("/{}", segments.join("/"));
    rebuilt.set_path(&new_path);
    Some(rebuilt.into())
}

fn derive_flussonic_template(source_url: &str) -> Option<String> {
    let mut parsed = Url::parse(source_url).ok()?;
    let mut segments = parsed
        .path_segments()
        .map(|it| it.map(ToString::to_string).collect::<Vec<_>>())?;
    let file_name = segments.pop()?;
    segments.push(format!("{FLUSSONIC_TEMPLATE}{}", extract_suffix_from_filename(&file_name)));
    parsed.set_path(&format!("/{}", segments.join("/")));
    Some(parsed.into())
}

fn append_query_template(source_url: &str, source_template: &str) -> Option<String> {
    let mut parsed = Url::parse(source_url).ok()?;
    let append = source_template.strip_prefix('?').unwrap_or(source_template);
    let mut merged = parsed.query().map(ToString::to_string).unwrap_or_default();
    if !merged.is_empty() && !append.is_empty() {
        merged.push('&');
    }
    merged.push_str(append);
    parsed.set_query((!merged.is_empty()).then_some(merged.as_str()));
    Some(parsed.into())
}

fn extract_suffix_from_filename(file_name: &str) -> &str {
    file_name
        .char_indices()
        .find_map(|(idx, ch)| (ch == '.').then_some(&file_name[idx..]))
        .unwrap_or_default()
}

fn is_append_like_query_source(mode: &str, source: &str) -> bool {
    mode_alias(mode) == "append" || source.starts_with('?')
}

fn derived_template_for_mode<'a>(source_url: &'a str, catchup: &'a CatchupProperties) -> Option<Cow<'a, str>> {
    let mode = catchup.mode.as_deref().unwrap_or_default();
    if let Some(source) = catchup.source.as_deref().filter(|source| !source.is_empty()) {
        return Some(if is_append_like_query_source(mode, source) {
            append_query_template(source_url, source).map(Cow::Owned)?
        } else {
            Cow::Borrowed(source)
        });
    }

    match mode_alias(mode) {
        "shift" => Some(Cow::Owned(format!("{source_url}{}", append_siptv_template(source_url)))),
        "xc" => derive_xc_template(source_url).map(Cow::Owned),
        "fs" => derive_flussonic_template(source_url).map(Cow::Owned),
        "vod" => Some(Cow::Borrowed("{catchup-id}")),
        _ => None,
    }
}

fn resolve_collectors(raw_query: Option<&str>) -> Result<Vec<(usize, String)>, TuliproxError> {
    let mut collectors = Vec::new();
    let Some(raw_query) = raw_query else {
        return Ok(collectors);
    };

    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        if key == M3U_CATCHUP_MARKER {
            continue;
        }
        let Some(idx) = key
            .strip_prefix(COLLECTOR_PREFIX)
            .and_then(|suffix| suffix.parse::<usize>().ok()) else {
            return Err(TuliproxError::Crypto(format!("Unsupported catchup query parameter '{key}'")));
        };
        collectors.push((idx, value.into_owned()));
    }
    collectors.sort_by_key(|(idx, _)| *idx);
    Ok(collectors)
}

fn render_template(segments: &[TemplateSegment], collectors: &[(usize, String)]) -> Result<String, TuliproxError> {
    let mut collector_idx = 0usize;
    let mut output = String::new();

    for segment in segments {
        match segment {
            TemplateSegment::Literal(literal) => output.push_str(literal),
            TemplateSegment::Placeholder(_) => {
                let Some((actual_idx, value)) = collectors.get(collector_idx) else {
                    return Err(TuliproxError::Crypto("Missing catchup collector value".to_string()));
                };
                if *actual_idx != collector_idx {
                    return Err(TuliproxError::Crypto("Catchup collector sequence is sparse".to_string()));
                }
                output.push_str(value);
                collector_idx += 1;
            }
        }
    }

    if collector_idx != collectors.len() {
        return Err(TuliproxError::Crypto("Unexpected extra catchup collector values".to_string()));
    }

    Ok(output)
}

pub fn build_m3u_catchup_rewrite(
    secret: &[u8; 16],
    base_url: &str,
    username: &str,
    target_id: u16,
    virtual_id: u32,
    source_url: &str,
    catchup: &CatchupProperties,
) -> Result<Option<M3uCatchupRewrite>, TuliproxError> {
    let Some(template) = derived_template_for_mode(source_url, catchup) else {
        return Ok(None);
    };
    let token = encode_m3u_catchup_token(
        secret,
        &M3uCatchupToken {
            username: username.to_string(),
            target_id,
            virtual_id,
        },
    )?;
    let segments = parse_template(template.as_ref());
    let placeholders = collect_placeholders(&segments);
    let append_mode = catchup
        .source
        .as_deref()
        .filter(|source| !source.is_empty())
        .is_some_and(|source| is_append_like_query_source(catchup.mode.as_deref().unwrap_or_default(), source));

    let source = build_local_source(base_url, "", &token, &placeholders, append_mode);
    let mode = if append_mode { "append" } else { "default" };
    Ok(Some(M3uCatchupRewrite { mode: mode.intern(), source: source.intern() }))
}

pub fn has_m3u_catchup_marker(raw_query: Option<&str>) -> bool {
    raw_query.is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| key == M3U_CATCHUP_MARKER)
    })
}

pub fn is_xtream_m3u_catchup_supported(source_url: &str, catchup: &CatchupProperties) -> bool {
    xtream_m3u_catchup_segments(source_url, catchup).is_some()
}

fn xtream_m3u_catchup_segments(
    source_url: &str,
    catchup: &CatchupProperties,
) -> Option<Vec<TemplateSegment>> {
    let template = derived_template_for_mode(source_url, catchup)?;
    let segments = parse_template(template.as_ref());
    let mut has_placeholder = false;
    for segment in &segments {
        if let TemplateSegment::Placeholder(value) = segment {
            has_placeholder = true;
            if !matches!(value.as_str(), "{utc}" | "{duration}") {
                return None;
            }
        }
    }
    has_placeholder.then_some(segments)
}

fn parse_xtream_bridge_start(start: &str) -> Result<i64, TuliproxError> {
    if let Ok(ts) = start.parse::<i64>() {
        return DateTime::from_timestamp(ts, 0).map(|_| ts).ok_or_else(|| {
            TuliproxError::RepositoryM3u(format!("Invalid Xtream start timestamp: {start}"))
        });
    }
    for fmt in XTREAM_BRIDGE_START_FORMATS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(start, fmt) {
            return Ok(dt.and_utc().timestamp());
        }
    }
    Err(TuliproxError::RepositoryM3u(format!(
        "Unsupported Xtream start time format: {start}"
    )))
}

pub fn resolve_xtream_m3u_catchup_url(
    source_url: &str,
    catchup: &CatchupProperties,
    start: &str,
    duration_minutes: &str,
) -> Result<ResolvedM3uCatchup, TuliproxError> {
    let segments = xtream_m3u_catchup_segments(source_url, catchup).ok_or_else(|| {
        TuliproxError::RepositoryM3u(
            "M3U catch-up template is not supported by the Xtream bridge".to_string(),
        )
    })?;
    let utc_start = parse_xtream_bridge_start(start)?;
    let minutes: u64 = duration_minutes.parse().map_err(|_| {
        TuliproxError::RepositoryM3u(format!("Invalid Xtream duration minutes: {duration_minutes}"))
    })?;
    let duration_secs = minutes.checked_mul(60).ok_or_else(|| {
        TuliproxError::RepositoryM3u("Xtream duration minutes overflow seconds".to_string())
    })?;

    let collectors: Vec<(usize, String)> = segments
        .iter()
        .filter_map(|segment| match segment {
            TemplateSegment::Placeholder(value) => Some(value.as_str()),
            TemplateSegment::Literal(_) => None,
        })
        .enumerate()
        .map(|(idx, p)| {
            let value = if p == "{utc}" {
                utc_start.to_string()
            } else {
                duration_secs.to_string()
            };
            (idx, value)
        })
        .collect();

    let url = render_template(&segments, &collectors)?;
    let discriminator = short_hash(&url);
    Ok(ResolvedM3uCatchup { url, discriminator })
}

pub fn resolve_m3u_catchup_url(
    source_url: &str,
    catchup: &CatchupProperties,
    raw_query: Option<&str>,
) -> Result<Option<ResolvedM3uCatchup>, TuliproxError> {
    let Some(template) = derived_template_for_mode(source_url, catchup) else {
        return Ok(None);
    };
    let segments = parse_template(template.as_ref());
    let placeholders = collect_placeholders(&segments);
    let collectors = resolve_collectors(raw_query)?;
    if collectors.len() != placeholders.len() {
        return Err(TuliproxError::Crypto(format!(
            "Catchup collector mismatch: expected {}, got {}",
            placeholders.len(),
            collectors.len()
        )));
    }

    let url = render_template(&segments, &collectors)?;
    let mut discriminator = url::form_urlencoded::Serializer::new(String::new());
    discriminator.append_pair("mode", catchup.mode.as_deref().unwrap_or("default"));
    for (idx, value) in collectors {
        discriminator.append_pair(&format!("{COLLECTOR_PREFIX}{idx}"), &value);
    }
    let discriminator = short_hash(&discriminator.finish());
    Ok(Some(ResolvedM3uCatchup { url, discriminator }))
}

#[cfg(test)]
mod tests {
    use super::{
        build_m3u_catchup_rewrite, has_m3u_catchup_marker, is_xtream_m3u_catchup_supported,
        resolve_m3u_catchup_url, resolve_xtream_m3u_catchup_url, M3U_CATCHUP_MARKER,
    };
    use shared::{error::TuliproxError, model::CatchupProperties, utils::Internable};

    #[test]
    fn explicit_append_rewrite_uses_live_route_marker() {
        let rewrite = build_m3u_catchup_rewrite(
            &[7u8; 16],
            "http://proxy.example",
            "alice",
            7,
            42,
            "http://provider.example/live/42.m3u8",
            &CatchupProperties {
                mode: Some("append".intern()),
                source: Some("?offset=-${offset}&utcstart=${timestamp}".intern()),
                ..CatchupProperties::default()
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(rewrite.mode.as_ref(), "append");
        assert!(rewrite.source.starts_with("?tuliprox-catchup="));
        assert!(rewrite.source.contains("&v0=${offset}"));
        assert!(rewrite.source.contains("&v1=${timestamp}"));
    }

    #[test]
    fn shift_rewrite_uses_direct_catchup_route() {
        let rewrite = build_m3u_catchup_rewrite(
            &[7u8; 16],
            "http://proxy.example",
            "alice",
            7,
            42,
            "http://provider.example/live/42.ts",
            &CatchupProperties {
                mode: Some("shift".intern()),
                ..CatchupProperties::default()
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(rewrite.mode.as_ref(), "default");
        assert!(rewrite.source.starts_with("http://proxy.example/m3u-catchup/"));
        assert!(rewrite.source.contains("?v0={utc}"));
        assert!(rewrite.source.contains("&v1={lutc}"));
    }

    #[test]
    fn resolve_explicit_append_template_roundtrips_collectors() {
        let resolved = resolve_m3u_catchup_url(
            "http://provider.example/live/42.m3u8",
            &CatchupProperties {
                mode: Some("append".intern()),
                source: Some("?offset=-${offset}&utcstart=${timestamp}".intern()),
                ..CatchupProperties::default()
            },
            Some("tuliprox-catchup=abc&v0=120&v1=1717200000"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            resolved.url,
            "http://provider.example/live/42.m3u8?offset=-120&utcstart=1717200000"
        );
        assert!(!resolved.discriminator.is_empty());
    }

    #[test]
    fn resolve_explicit_append_template_merges_into_existing_query() {
        let resolved = resolve_m3u_catchup_url(
            "http://provider.example/live/42.m3u8?token=abc",
            &CatchupProperties {
                mode: Some("append".intern()),
                source: Some("?offset=-${offset}&utcstart=${timestamp}".intern()),
                ..CatchupProperties::default()
            },
            Some("tuliprox-catchup=abc&v0=120&v1=1717200000"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            resolved.url,
            "http://provider.example/live/42.m3u8?token=abc&offset=-120&utcstart=1717200000"
        );
    }

    #[test]
    fn resolve_default_mode_query_template_keeps_base_stream_url() {
        let resolved = resolve_m3u_catchup_url(
            "http://provider.example/live/42.ts",
            &CatchupProperties {
                mode: Some("default".intern()),
                source: Some("?playseek=${timestamp}&duration=${duration}".intern()),
                ..CatchupProperties::default()
            },
            Some("v0=1717200000&v1=120"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            resolved.url,
            "http://provider.example/live/42.ts?playseek=1717200000&duration=120"
        );
    }

    #[test]
    fn default_mode_query_template_rewrite_uses_append_route_marker() {
        let rewrite = build_m3u_catchup_rewrite(
            &[7u8; 16],
            "http://proxy.example",
            "alice",
            7,
            42,
            "http://provider.example/live/42.ts",
            &CatchupProperties {
                mode: Some("default".intern()),
                source: Some("?playseek=${timestamp}&duration=${duration}".intern()),
                ..CatchupProperties::default()
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(rewrite.mode.as_ref(), "append");
        assert!(rewrite.source.starts_with("?tuliprox-catchup="));
        assert!(rewrite.source.contains("&v0=${timestamp}"));
        assert!(rewrite.source.contains("&v1=${duration}"));
    }

    #[test]
    fn resolve_shift_template_roundtrips_collectors() {
        let resolved = resolve_m3u_catchup_url(
            "http://provider.example/live/42.ts",
            &CatchupProperties {
                mode: Some("shift".intern()),
                ..CatchupProperties::default()
            },
            Some("v0=20240101120000&v1=20240101130000"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            resolved.url,
            "http://provider.example/live/42.ts?utc=20240101120000&lutc=20240101130000"
        );
    }

    #[test]
    fn discriminator_changes_when_collectors_change() {
        let first = resolve_m3u_catchup_url(
            "http://provider.example/live/42.ts",
            &CatchupProperties {
                mode: Some("shift".intern()),
                ..CatchupProperties::default()
            },
            Some("v0=20240101120000&v1=20240101130000"),
        )
        .unwrap()
        .unwrap();
        let second = resolve_m3u_catchup_url(
            "http://provider.example/live/42.ts",
            &CatchupProperties {
                mode: Some("shift".intern()),
                ..CatchupProperties::default()
            },
            Some("v0=20240101140000&v1=20240101150000"),
        )
        .unwrap()
        .unwrap();

        assert_ne!(first.discriminator, second.discriminator);
    }

    #[test]
    fn catchup_marker_detection_is_explicit() {
        assert!(has_m3u_catchup_marker(Some(&format!("{M3U_CATCHUP_MARKER}=abc&v0=1"))));
        assert!(!has_m3u_catchup_marker(Some("v0=1")));
        assert!(!has_m3u_catchup_marker(None));
    }

    #[test]
    fn xtream_bridge_resolves_flussonic_utc_and_duration() {
        let catchup = CatchupProperties {
            mode: Some("flussonic".intern()),
            source: Some("http://provider.example/channel/video-{utc}-{duration}.m3u8".intern()),
            ..CatchupProperties::default()
        };
        let resolved = resolve_xtream_m3u_catchup_url(
            "http://provider.example/channel/index.m3u8",
            &catchup,
            "2024-01-01:00-00",
            "60",
        )
        .expect("valid Flussonic template");
        assert_eq!(
            resolved.url,
            "http://provider.example/channel/video-1704067200-3600.m3u8"
        );
        assert!(!resolved.discriminator.is_empty());
    }

    #[test]
    fn xtream_bridge_rejects_unsupported_placeholder() {
        let catchup = CatchupProperties {
            mode: Some("flussonic".intern()),
            source: Some("http://provider.example/channel/${timestamp}.m3u8".intern()),
            ..CatchupProperties::default()
        };
        assert!(!is_xtream_m3u_catchup_supported(
            "http://provider.example/channel/index.m3u8",
            &catchup
        ));
        let err = resolve_xtream_m3u_catchup_url(
            "http://provider.example/channel/index.m3u8",
            &catchup,
            "2024-01-01:00-00",
            "60",
        )
        .unwrap_err();
        assert!(matches!(err, TuliproxError::RepositoryM3u(_)));
    }

    #[test]
    fn xtream_bridge_rejects_invalid_start_time() {
        let catchup = CatchupProperties {
            mode: Some("flussonic".intern()),
            source: Some("http://provider.example/channel/video-{utc}-{duration}.m3u8".intern()),
            ..CatchupProperties::default()
        };
        let err = resolve_xtream_m3u_catchup_url(
            "http://provider.example/channel/index.m3u8",
            &catchup,
            "not-a-timestamp",
            "60",
        )
        .unwrap_err();
        assert!(matches!(err, TuliproxError::RepositoryM3u(_)));
    }

    #[test]
    fn xtream_bridge_rejects_overflowing_duration_minutes() {
        let catchup = CatchupProperties {
            mode: Some("flussonic".intern()),
            source: Some("http://provider.example/channel/video-{utc}-{duration}.m3u8".intern()),
            ..CatchupProperties::default()
        };
        let err = resolve_xtream_m3u_catchup_url(
            "http://provider.example/channel/index.m3u8",
            &catchup,
            "2024-01-01:00-00",
            &u64::MAX.to_string(),
        )
        .unwrap_err();
        assert!(matches!(err, TuliproxError::RepositoryM3u(_)));
    }

    #[test]
    fn xtream_bridge_accepts_unix_seconds_start() {
        let catchup = CatchupProperties {
            mode: Some("flussonic".intern()),
            source: Some("http://provider.example/channel/video-{utc}-{duration}.m3u8".intern()),
            ..CatchupProperties::default()
        };
        let resolved = resolve_xtream_m3u_catchup_url(
            "http://provider.example/channel/index.m3u8",
            &catchup,
            "1704067200",
            "60",
        )
        .expect("unix seconds start");
        assert_eq!(
            resolved.url,
            "http://provider.example/channel/video-1704067200-3600.m3u8"
        );
    }
}
