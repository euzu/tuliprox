use crate::model::{Config, ConfigInput};
use crate::utils::request::DynReader;
use shared::model::{
    CatchupAttribute, CatchupProperties, LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader,
    PlaylistItemType, StreamProperties, XtreamCluster,
};
use shared::utils::{extract_id_from_url, extract_numeric_id_from_url, Internable};
use shared::defaults::{default_supported_video_extensions};
use std::borrow::BorrowMut;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use indexmap::IndexMap;
use crate::repository::CategoryKey;

// other implementations like calculating text_distance on all titles took too much time
// we keep it now as simple as possible and less memory intensive.
fn get_title_group(text: &Arc<str>) -> Arc<str> {
    let mut combination = String::new();
    let mut in_word = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if !in_word && !combination.is_empty() {
                combination.push(' ');
            }
            in_word = true;
            combination.push(c);
        } else {
            if in_word && combination.len() > 2 {
                return combination.intern();
            }
            in_word = false;
        }
    }
    if combination.len() > 2 {
        return combination.intern();
    }
    text.clone()
}

/// Reads a quoted value into the stack and returns the start offset.
/// The value is `&stack[offset..]`. Caller must call `stack.truncate(offset)` after use.
#[inline]
fn token_value(stack: &mut String, it: &mut std::str::Chars) -> usize {
    let offset = stack.len();
    // `any()` intentionally consumes chars from the iterator, advancing it past the opening '"'.
    // Returns true when a quote is found, so read_value can extract the content until the closing '"'.
    if it.any(|ch| ch == '"') {
        read_value(stack, it);
    }
    offset
}

/// Reads a quoted or comma-delimited value into the stack and returns the start offset.
fn read_value(stack: &mut String, it: &mut std::str::Chars) {
    for c in it.skip_while(|c| c.is_whitespace()) {
        if c == '"' {
            break;
        }
        stack.push(c);
    }
}

/// Reads a comma-delimited value (title) into the stack and returns the start offset.
#[inline]
fn title_value(stack: &mut String, it: &mut std::str::Chars) -> usize {
    let offset = stack.len();
    read_value(stack, it);
    offset
}

/// Reads characters into the stack until `stop_char` is reached.
/// Returns `Some(offset)` where `&stack[offset..]` is the token, or `None` if empty.
/// Caller must call `stack.truncate(offset)` after use.
fn token_till(stack: &mut String, it: &mut std::str::Chars, stop_char: char, start_with_alpha: bool) -> Option<usize> {
    let offset = stack.len();
    let mut skip_non_alpha = start_with_alpha;

    for ch in it.by_ref() {
        if ch == stop_char {
            break;
        }
        if stack.len() == offset && ch.is_whitespace() {
            continue;
        }

        if skip_non_alpha {
            if ch.is_alphabetic() {
                skip_non_alpha = false;
            } else {
                continue;
            }
        }
        stack.push(ch);
    }

    if stack.len() == offset {
        None
    } else {
        Some(offset)
    }
}

#[inline]
fn skip_digit(it: &mut std::str::Chars) -> Option<char> {
    loop {
        {
            let c = it.next()?;
            if !(c == '-' || c == '+' || c.is_ascii_digit()) {
                return Some(c);
            }
        }
    }
}

fn create_empty_playlistitem_header(input_name: &Arc<str>, url: String) -> PlaylistItemHeader {
    PlaylistItemHeader {
        url: Arc::from(url),
        category_id: 0,
        input_name: Arc::clone(input_name),
        ..Default::default()
    }
}

enum M3uToken {
    ProviderId,
    TvgChno,
    GroupTitle,
    TvgId,
    TvgName,
    TvgLogo,
    TvgLogoSmall,
    ParentCode,
    AudioTrack,
    TimeShift,
    TvgRec,
    Catchup,
    CatchupDays,
    CatchupSource,
    CatchupTime,
    CatchupCorrection,
    CatchupType,
    CatchupExtra,
    PossibleId,
    Unknown,
}

#[inline]
fn eq_ascii(bytes: &[u8], expected: &[u8]) -> bool { bytes.eq_ignore_ascii_case(expected) }

#[inline]
fn classify_possible_id(bytes: &[u8]) -> M3uToken {
    if eq_ascii(bytes, b"id")
        || (bytes.len() > 2 && !eq_ascii(&bytes[..3], b"tvg") && eq_ascii(&bytes[bytes.len() - 2..], b"id"))
    {
        M3uToken::PossibleId
    } else {
        M3uToken::Unknown
    }
}

fn classify_token(t: &str) -> M3uToken {
    let bytes = t.as_bytes();

    match bytes.len() {
        2 => classify_possible_id(bytes),
        4 => {
            if eq_ascii(bytes, b"cuid") {
                M3uToken::ProviderId
            } else {
                M3uToken::Unknown
            }
        }
        6 => {
            if eq_ascii(bytes, b"xui-id") {
                M3uToken::ProviderId
            } else if eq_ascii(bytes, b"tvg-id") {
                M3uToken::TvgId
            } else {
                classify_possible_id(bytes)
            }
        }
        7 => {
            if eq_ascii(bytes, b"tvg-rec") {
                M3uToken::TvgRec
            } else if eq_ascii(bytes, b"catchup") {
                M3uToken::Catchup
            } else {
                classify_possible_id(bytes)
            }
        }
        8 => {
            if eq_ascii(bytes, b"tvg-chno") {
                M3uToken::TvgChno
            } else if eq_ascii(bytes, b"tvg-name") {
                M3uToken::TvgName
            } else if eq_ascii(bytes, b"tvg-logo") {
                M3uToken::TvgLogo
            } else {
                classify_possible_id(bytes)
            }
        }
        9 => {
            if eq_ascii(bytes, b"timeshift") {
                M3uToken::TimeShift
            } else {
                classify_possible_id(bytes)
            }
        }
        10 => {
            if eq_ascii(bytes, b"catchup-id") {
                M3uToken::CatchupExtra
            } else {
                classify_possible_id(bytes)
            }
        }
        11 => {
            if eq_ascii(bytes, b"group-title") {
                M3uToken::GroupTitle
            } else if eq_ascii(bytes, b"parent-code") {
                M3uToken::ParentCode
            } else if eq_ascii(bytes, b"audio-track") {
                M3uToken::AudioTrack
            } else {
                classify_possible_id(bytes)
            }
        }
        12 => {
            if eq_ascii(bytes, b"catchup-days") {
                M3uToken::CatchupDays
            } else if eq_ascii(bytes, b"catchup-type") {
                M3uToken::CatchupType
            } else if eq_ascii(bytes, b"catchup-time") {
                M3uToken::CatchupTime
            } else {
                classify_possible_id(bytes)
            }
        }
        14 => {
            if eq_ascii(bytes, b"tvg-logo-small") {
                M3uToken::TvgLogoSmall
            } else if eq_ascii(bytes, b"catchup-source") {
                M3uToken::CatchupSource
            } else {
                classify_possible_id(bytes)
            }
        }
        18 => {
            if eq_ascii(bytes, b"catchup-correction") {
                M3uToken::CatchupCorrection
            } else {
                classify_possible_id(bytes)
            }
        }
        _ => {
            if bytes.len() > 8 && eq_ascii(&bytes[..8], b"catchup-") {
                M3uToken::CatchupExtra
            } else {
                classify_possible_id(bytes)
            }
        }
    }
}

fn ensure_live_stream_properties(header: &mut PlaylistItemHeader) -> &mut LiveStreamProperties {
    if !matches!(header.additional_properties.as_ref(), Some(StreamProperties::Live(_))) {
        header.additional_properties = Some(StreamProperties::Live(Box::default()));
    }

    let Some(StreamProperties::Live(live)) = header.additional_properties.as_mut() else {
        unreachable!("additional_properties just initialized as live");
    };

    if live.name.is_empty() {
        if !header.name.is_empty() {
            live.name.clone_from(&header.name);
        } else if !header.title.is_empty() {
            live.name.clone_from(&header.title);
        }
    }
    if live.epg_channel_id.is_none() {
        live.epg_channel_id.clone_from(&header.epg_channel_id);
    }

    live
}

fn apply_catchup_properties(header: &mut PlaylistItemHeader, mut catchup: CatchupProperties, default_correction: Option<&Arc<str>>) {
    let has_capability = catchup.mode.is_some()
        || catchup.days.is_some()
        || catchup.source.is_some()
        || catchup.time.is_some()
        || catchup.catchup_type.is_some()
        || !catchup.extra_attributes.is_empty();

    if !has_capability {
        return;
    }

    if catchup.correction.is_none() {
        catchup.correction = default_correction.cloned();
    }

    let live = ensure_live_stream_properties(header);
    live.tv_archive = Some(1);
    live.tv_archive_duration = catchup
        .days
        .as_deref()
        .and_then(|days| days.trim().parse::<i32>().ok())
        .or(live.tv_archive_duration);
    live.catchup = Some(catchup);
}

#[allow(clippy::too_many_lines)]
fn process_header_internal(
    input_name: &Arc<str>,
    video_suffixes: &[String],
    content: &str,
    url: String,
    default_catchup_correction: Option<&Arc<str>>,
) -> PlaylistItemHeader {
    let url_types = if video_suffixes.iter().any(|suffix| url.ends_with(suffix)) {
        // TODO find Series based on group or configured names
        Some((XtreamCluster::Video, PlaylistItemType::Video))
    } else {
        None
    };

    let mut plih = create_empty_playlistitem_header(input_name, url);
    let mut it = content.chars();
    let mut stack = String::with_capacity(64);
    let is_extinf = token_till(&mut stack, &mut it, ':', false)
        .is_some_and(|off| stack[off..].eq_ignore_ascii_case("#EXTINF"));
    stack.clear();
    if is_extinf {
        let mut provider_id = None::<String>;
        let mut fallback_id = None::<String>;
        let mut catchup = CatchupProperties::default();
        let mut c = skip_digit(&mut it);
        while let Some(chr) = c {
            match chr {
                _ if chr.is_whitespace() => {}
                ',' => {
                    let off = title_value(&mut stack, &mut it);
                    plih.title = stack[off..].intern();
                    stack.truncate(off);
                }
                _ => {
                    let tok_start = stack.len();
                    stack.push(chr);
                    if token_till(&mut stack, &mut it, '=', true).is_some() {
                        let token = classify_token(&stack[tok_start..]);
                        let token_name = stack[tok_start..].to_owned();
                        stack.clear();
                        let val_off = token_value(&mut stack, &mut it);
                        match token {
                            M3uToken::ProviderId if stack.len() > val_off => {
                                provider_id = Some(stack[val_off..].to_owned());
                            }
                            M3uToken::TvgChno => plih.chno = stack[val_off..].parse::<u32>().unwrap_or(0),
                            M3uToken::GroupTitle => plih.group = stack[val_off..].intern(),
                            M3uToken::TvgId => plih.epg_channel_id = if stack.len() == val_off { None } else { Some(stack[val_off..].intern()) },
                            M3uToken::TvgName => plih.name = stack[val_off..].intern(),
                            M3uToken::TvgLogo => plih.logo = stack[val_off..].intern(),
                            M3uToken::TvgLogoSmall => plih.logo_small = stack[val_off..].intern(),
                            M3uToken::ParentCode => plih.parent_code = stack[val_off..].intern(),
                            M3uToken::AudioTrack => plih.audio_track = stack[val_off..].intern(),
                            M3uToken::TimeShift => plih.time_shift = stack[val_off..].intern(),
                            M3uToken::TvgRec => plih.rec = stack[val_off..].intern(),
                            M3uToken::Catchup if stack.len() > val_off => catchup.mode = Some(stack[val_off..].intern()),
                            M3uToken::CatchupDays if stack.len() > val_off => catchup.days = Some(stack[val_off..].intern()),
                            M3uToken::CatchupSource if stack.len() > val_off => {
                                catchup.source = Some(stack[val_off..].intern());
                            }
                            M3uToken::CatchupTime if stack.len() > val_off => catchup.time = Some(stack[val_off..].intern()),
                            M3uToken::CatchupCorrection if stack.len() > val_off => {
                                catchup.correction = Some(stack[val_off..].intern());
                            }
                            M3uToken::CatchupType if stack.len() > val_off => {
                                catchup.catchup_type = Some(stack[val_off..].intern());
                            }
                            M3uToken::CatchupExtra if stack.len() > val_off => catchup.extra_attributes.push(CatchupAttribute {
                                name: token_name.intern(),
                                value: stack[val_off..].intern(),
                            }),
                            // Unknown panel-specific ID fields (e.g. "stream-id", "channel-uid")
                            M3uToken::PossibleId
                                if fallback_id.is_none()
                                    && stack.len() > val_off
                                    && stack[val_off..].bytes().all(|b| b.is_ascii_digit()) =>
                            {
                                fallback_id = Some(stack[val_off..].to_owned());
                            }
                            _ => {}
                        }
                        stack.clear();
                    }
                }
            }
            c = it.next();
        }

        if let Some(numeric_url_id) = extract_numeric_id_from_url(&plih.url).filter(|&id| id > 0) {
            // Numeric ID extracted from URL is always the authoritative provider ID.
            plih.id = numeric_url_id.to_string().intern();
        } else if let Some(pid) = provider_id {
            plih.id = pid.intern();
        } else if let Some(fid) = fallback_id {
            plih.id = fid.intern();
        } else {
            plih.id = extract_id_from_url(&plih.url).intern();
        }
        apply_catchup_properties(&mut plih, catchup, default_catchup_correction);
    }
    if let Some((url_cluster, url_item_type)) = url_types {
        plih.xtream_cluster = url_cluster;
        plih.item_type = url_item_type;
    }

    {
        let header = plih.borrow_mut();
        if header.name.is_empty() {
            if !header.title.is_empty() {
                header.name = header.title.clone();
            } else if !header.id.is_empty() {
                header.name = header.id.clone();
                header.title = header.id.clone();
            }
        }
    }

    plih.freeze_input_stream_id();
    plih
}

#[cfg(test)]
fn process_header(input_name: &Arc<str>, video_suffixes: &[String], content: &str, url: String) -> PlaylistItemHeader {
    process_header_internal(input_name, video_suffixes, content, url, None)
}

fn parse_extm3u_catchup_correction(attributes: &str) -> Option<Arc<str>> {
    let mut it = attributes.chars();
    let mut stack = String::with_capacity(32);
    while let Some(chr) = it.next() {
        if chr.is_whitespace() {
            continue;
        }

        let tok_start = stack.len();
        stack.push(chr);
        if token_till(&mut stack, &mut it, '=', true).is_none() {
            stack.clear();
            continue;
        }
        let is_catchup_correction = stack[tok_start..].eq_ignore_ascii_case("catchup-correction");
        stack.clear();
        let val_off = token_value(&mut stack, &mut it);
        if is_catchup_correction && stack.len() > val_off {
            return Some(stack[val_off..].intern());
        }
        stack.clear();
    }
    None
}

fn parse_extvlcopt_user_agent(line: &str) -> Option<&str> {
    let (prefix, option) = line.trim().split_once(':')?;
    if !prefix.eq_ignore_ascii_case("#EXTVLCOPT") {
        return None;
    }
    let (name, value) = option.split_once('=')?;
    if !name.trim().eq_ignore_ascii_case("http-user-agent") {
        return None;
    }
    Some(value.trim())
}

pub async fn consume_m3u<F: FnMut(PlaylistItem)>(cfg: &Config, input: &ConfigInput, lines: DynReader, mut visit: F) {
    let mut header: Option<String> = None;
    let mut group: Option<Arc<str>> = None;
    let mut source_user_agent: Option<Arc<str>> = None;
    let mut default_catchup_correction: Option<Arc<str>> = None;
    let input_name = &input.name;

    let video_suffixes = match cfg.video.as_ref() {
        Some(config) => {
            config.extensions.iter().map(Clone::clone).collect::<Vec<String>>()
        }
        None => default_supported_video_extensions()
        };
    let mut lines = tokio::io::BufReader::new(lines).lines();
    let mut ord_counter: u32 = 1;
    while let Ok(Some(line)) = lines.next_line().await {
        let bytes = line.as_bytes();
        if let Some(b'#') = bytes.first().copied() {
            if bytes.starts_with(b"#EXTINF") {
                header = Some(line);
                source_user_agent = None;
                continue;
            }
            if let Some(value) = parse_extvlcopt_user_agent(&line) {
                if header.is_some() {
                    source_user_agent = (!value.is_empty()).then(|| value.intern());
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("#EXTM3U") {
                default_catchup_correction = parse_extm3u_catchup_correction(rest);
                continue;
            }
            if let Some(rest) = line.strip_prefix("#EXTGRP:") {
                group = Some(rest.intern());
                continue;
            }
            continue;
        }
        let group_value = group.take();
        if let Some(header_value) = header.take() {
            let mut item = PlaylistItem {
                header: process_header_internal(
                    input_name,
                    &video_suffixes,
                    &header_value,
                    line,
                    default_catchup_correction.as_ref(),
                ),
            };
            let header = &mut item.header;
            header.source_user_agent = source_user_agent.take();
            header.source_ordinal = ord_counter;
            ord_counter += 1;
            if header.group.is_empty() {
                if let Some(group_value) = group_value {
                    header.group = group_value;
                } else {
                    header.group = get_title_group(&header.title);
                }
            }
            visit(item);
        }
    }
}

pub async fn parse_m3u(cfg: &Config, input: &ConfigInput, lines: DynReader) -> Vec<PlaylistGroup>
{
    let mut group_map: IndexMap<CategoryKey, Vec<PlaylistItem>> = IndexMap::new();
    consume_m3u(cfg, input, lines, |item| {
        let key = {
            let header = &item.header;
            let normalized_group = shared::utils::deunicode_string(&header.group).to_lowercase().intern();
            (header.xtream_cluster, normalized_group)
        };
        group_map.entry(key).or_default().push(item);
    }).await;

    let mut grp_id = 0;
    group_map.into_values().filter_map(|channels| {
        // create a group based on the first playlist item
        let channel = channels.first();
        if let Some((cluster, group_title)) = channel.map(|pli|
            (pli.header.xtream_cluster, &pli.header.group)) {
            grp_id += 1;
            Some(PlaylistGroup { id: grp_id, xtream_cluster: cluster, title: group_title.clone(), channels })
        } else {
            None
        }
    }).collect()
}

#[cfg(test)]
mod test {
    use crate::model::{Config, ConfigInput};
    use crate::utils::request::DynReader;
    use shared::{
        model::StreamProperties,
        utils::Internable,
    };
    use crate::processing::parser::m3u::{classify_token, process_header, M3uToken};
    use tokio::io::AsyncWriteExt;

    fn make_reader(content: &str) -> DynReader {
        let (mut writer, reader) = tokio::io::duplex(content.len().max(4096));
        let bytes = content.as_bytes().to_vec();
        tokio::spawn(async move {
            writer.write_all(&bytes).await.unwrap();
            writer.shutdown().await.unwrap();
        });
        Box::pin(reader)
    }

    fn test_input() -> ConfigInput {
        ConfigInput {
            name: "input".intern(),
            url: "http://provider.example".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            ..ConfigInput::default()
        }
    }

    #[test]
    fn test_classify_token_length_dispatch_keeps_expected_mapping() {
        assert!(matches!(classify_token("xui-id"), M3uToken::ProviderId));
        assert!(matches!(classify_token("CUID"), M3uToken::ProviderId));
        assert!(matches!(classify_token("tvg-chno"), M3uToken::TvgChno));
        assert!(matches!(classify_token("group-title"), M3uToken::GroupTitle));
        assert!(matches!(classify_token("TVG-ID"), M3uToken::TvgId));
        assert!(matches!(classify_token("tvg-name"), M3uToken::TvgName));
        assert!(matches!(classify_token("tvg-logo"), M3uToken::TvgLogo));
        assert!(matches!(classify_token("tvg-logo-small"), M3uToken::TvgLogoSmall));
        assert!(matches!(classify_token("parent-code"), M3uToken::ParentCode));
        assert!(matches!(classify_token("audio-track"), M3uToken::AudioTrack));
        assert!(matches!(classify_token("timeshift"), M3uToken::TimeShift));
        assert!(matches!(classify_token("tvg-rec"), M3uToken::TvgRec));
        assert!(matches!(classify_token("catchup"), M3uToken::Catchup));
        assert!(matches!(classify_token("catchup-days"), M3uToken::CatchupDays));
        assert!(matches!(classify_token("catchup-source"), M3uToken::CatchupSource));
        assert!(matches!(classify_token("catchup-time"), M3uToken::CatchupTime));
        assert!(matches!(classify_token("catchup-correction"), M3uToken::CatchupCorrection));
        assert!(matches!(classify_token("catchup-type"), M3uToken::CatchupType));
        assert!(matches!(classify_token("catchup-extra"), M3uToken::CatchupExtra));
        assert!(matches!(classify_token("stream-id"), M3uToken::PossibleId));
        assert!(matches!(classify_token("id"), M3uToken::PossibleId));
        assert!(matches!(classify_token("foo"), M3uToken::Unknown));
    }

    #[test]
    fn test_process_header_1() {
        let input = "hello".intern();
        let video_suffixes = Vec::new();
        let url = "http://hello.de/live/user/pass/70001.ts";
        let line = r#"#EXTINF:-1 channel-id="abc-seven" tvg-id="abc-seven" tvg-logo="https://abc.nz/.images/seven.png" tvg-chno="7" group-title="Sydney" , Seven"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.title, "Seven".intern());
        // tvg-id is preserved as epg_channel_id, id falls back to numeric url segment
        assert_eq!(pli.epg_channel_id, Some("abc-seven".intern()));
        assert_eq!(pli.id, "70001".intern());
        assert_eq!(pli.logo, "https://abc.nz/.images/seven.png".intern());
        assert_eq!(pli.chno, 7);
        assert_eq!(&*pli.group, "Sydney");
    }

    #[test]
    fn test_process_header_2() {
        let input = "hello".intern();
        let video_suffixes = Vec::new();
        let url = "http://hello.de/live/user/pass/70002.ts";
        let line = r#"#EXTINF:-1 channel-id="abc-seven" tvg-id="abc-seven" tvg-logo="https://abc.nz/.images/seven.png" tvg-chno="7" group-title="Sydney", Seven"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.title, "Seven".intern());
        assert_eq!(pli.epg_channel_id, Some("abc-seven".intern()));
        assert_eq!(pli.id, "70002".intern());
        assert_eq!(pli.logo, "https://abc.nz/.images/seven.png".intern());
        assert_eq!(pli.chno, 7);
        assert_eq!(&*pli.group, "Sydney");
    }

    #[test]
    fn test_process_header_cuid_format() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let url = "http://line.trx-ott.com/live/18be61b480/fc19249ec409/1905905.ts";
        let line = r#"#EXTINF:0 CUID="12046" tvg-name="UK-NOWTV| SKY CRIME FHD" tvg-id="skycrime.uk" tvg-logo="https://logo.m3uassets.com/skycrime.png" group-title="🔪Murder Mystery",UK-NOWTV| SKY CRIME FHD"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.name, "UK-NOWTV| SKY CRIME FHD".intern());
        assert_eq!(pli.title, "UK-NOWTV| SKY CRIME FHD".intern());
        assert_eq!(pli.id, "1905905".intern()); // URL id is master; CUID is only fallback
        assert_eq!(pli.logo, "https://logo.m3uassets.com/skycrime.png".intern());
        assert_eq!(&*pli.group, "🔪Murder Mystery");
        assert_eq!(pli.epg_channel_id, Some("skycrime.uk".intern()));
    }

    #[test]
    fn test_process_header_tvg_id_uses_url_id_fallback() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        // Numeric last segment in URL -> extract_id_from_url returns "1905905"
        let url = "http://line.trx-ott.com/live/user/pass/1905905.ts";
        let line = r#"#EXTINF:-1 tvg-id="skycrime.uk" tvg-name="SKY CRIME" group-title="Crime",SKY CRIME"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.id, "1905905".intern()); // url_id used as fallback
        assert_eq!(pli.epg_channel_id, Some("skycrime.uk".intern())); // tvg-id preserved for EPG
    }

    #[test]
    fn test_process_header_tvg_id_preserves_mixed_case() {
        // Supersedes #688: the M3U parser no longer lowercases tvg-id. EPG matching is
        // now case-insensitive, so the epg_channel_id keeps its original source case and
        // the M3U/XMLTV output preserves it.
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let url = "http://line.trx-ott.com/live/user/pass/1905905.ts";
        let line = r#"#EXTINF:-1 tvg-id="CNN.us" tvg-name="CNN" group-title="News",CNN"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.epg_channel_id, Some("CNN.us".intern())); // original case preserved, not lowercased
    }

    #[test]
    fn test_process_header_no_tvg_id_no_provider_id() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let url = "http://example.com/live/user/pass/12345.ts";
        let line = r#"#EXTINF:-1 tvg-name="Test Channel" group-title="Group",Test Channel"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.id, "12345".intern()); // url_id as sole fallback
        assert_eq!(pli.epg_channel_id, None); // no tvg-id -> no epg_channel_id
    }

    #[test]
    fn test_process_header_expiring_query_params_id_fallback() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let line = r#"#EXTINF:-1 tvg-name="Test Channel" group-title="Group",Test Channel"#;

        let pli = process_header(
            &input,
            &video_suffixes,
            line,
            "http://example.com/live/user/pass/1905905.ts?expires=1712345678&token=alpha".to_string(),
        );
        assert_eq!(pli.id, "1905905".intern());
        assert_eq!(pli.epg_channel_id, None);

        let pli_variant = process_header(
            &input,
            &video_suffixes,
            line,
            "http://example.com/live/user/pass/1905905.ts?expires=1719999999&token=beta".to_string(),
        );
        assert_eq!(pli_variant.id, "1905905".intern());
        assert_eq!(pli_variant.epg_channel_id, None);
    }

    #[test]
    fn test_process_header_xui_id() {
        let input = "hello".intern();
        let video_suffixes = Vec::new();
        let url = "http://hello.de/hello.ts";
        let line = r#"#EXTINF:-1 tvg-id="abc-seven" xui-id="provider-123" group-title="Sydney", Seven"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.title, "Seven".intern());
        assert_eq!(pli.id, "provider-123".intern()); // URL has no numeric id, xui-id used as fallback
        assert_eq!(pli.epg_channel_id, Some("abc-seven".intern())); // Should preserve original tvg-id
        assert_eq!(&*pli.group, "Sydney");
    }

    #[test]
    fn test_process_header_unknown_numeric_id_field() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let url = "http://example.com/live/user/pass/99999.ts";
        // "stream-id" is an unknown field ending in "id" with a numeric value
        let line = r#"#EXTINF:-1 stream-id="55555" tvg-name="Test Channel" group-title="Group",Test Channel"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.id, "99999".intern()); // URL numeric id is master, stream-id is only fallback
        assert_eq!(pli.epg_channel_id, None);
    }

    #[test]
    fn test_process_header_unknown_id_non_numeric_ignored() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let url = "http://example.com/live/user/pass/99999.ts";
        // "channel-id" has a non-numeric value, should be ignored as fallback
        let line = r#"#EXTINF:-1 channel-id="abc-def" tvg-name="Test" group-title="G",Test"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.id, "99999".intern()); // falls through to url_id
    }

    #[test]
    fn test_process_header_explicit_cuid_overrides_fallback_id() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let url = "http://example.com/live/user/pass/99999.ts";
        // Both CUID (explicit) and stream-id (fallback) present: CUID wins
        let line = r#"#EXTINF:-1 stream-id="55555" CUID="77777" tvg-name="Test" group-title="G",Test"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.id, "99999".intern()); // URL numeric id is master, CUID/stream-id are only fallbacks
    }

    #[test]
    fn test_process_header_preserves_catchup_attributes() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let url = "http://provider.example/live/user/pass/99999.ts";
        let line = r#"#EXTINF:-1 tvg-id="channel1" catchup="append" catchup-days="7" catchup-source="?offset=-${offset}&utcstart=${timestamp}" catchup-correction="-1.5" catchup-type="xc" catchup-extra="keep",Channel 1"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        let Some(StreamProperties::Live(live_props)) = pli.additional_properties else {
            panic!("expected live stream properties");
        };
        let catchup = live_props.catchup.expect("catchup should be parsed");
        assert_eq!(catchup.mode.as_deref(), Some("append"));
        assert_eq!(catchup.days.as_deref(), Some("7"));
        assert_eq!(catchup.source.as_deref(), Some("?offset=-${offset}&utcstart=${timestamp}"));
        assert_eq!(catchup.correction.as_deref(), Some("-1.5"));
        assert_eq!(catchup.catchup_type.as_deref(), Some("xc"));
        assert_eq!(catchup.extra_attributes.len(), 1);
        assert_eq!(catchup.extra_attributes[0].name.as_ref(), "catchup-extra");
        assert_eq!(live_props.tv_archive, Some(1));
        assert_eq!(live_props.tv_archive_duration, Some(7));
    }

    #[test]
    fn test_process_header_applies_extm3u_default_catchup_correction() {
        let input = "test".intern();
        let video_suffixes = Vec::new();
        let line = r#"#EXTINF:-1 catchup="append" catchup-source="?offset=-${offset}",Channel 1"#;
        let pli = super::process_header_internal(
            &input,
            &video_suffixes,
            line,
            "http://provider.example/live/user/pass/99999.ts".to_string(),
            Some(&"-2.0".intern()),
        );

        let Some(StreamProperties::Live(live_props)) = pli.additional_properties else {
            panic!("expected live stream properties");
        };
        let catchup = live_props.catchup.expect("catchup should be parsed");
        assert_eq!(catchup.correction.as_deref(), Some("-2.0"));
    }

    #[test]
    fn parse_extm3u_header_catchup_correction_from_attribute_tail() {
        let correction = super::parse_extm3u_catchup_correction(r#" catchup-correction="-2.0""#);

        assert_eq!(correction.as_deref(), Some("-2.0"));
    }

    #[tokio::test]
    async fn consume_m3u_applies_extgrp_to_following_item() {
        let content = concat!(
            "#EXTM3U\n",
            "#EXTINF:-1,Channel 1\n",
            "#EXTGRP:Sports\n",
            "http://provider.example/live/user/pass/1.ts\n",
        );
        let mut items = Vec::new();

        super::consume_m3u(&Config::default(), &test_input(), make_reader(content), |item| items.push(item)).await;

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].header.group.as_ref(), "Sports");
    }

    #[tokio::test]
    async fn consume_m3u_scopes_extvlcopt_user_agent_to_one_item() {
        let content = concat!(
            "#EXTM3U\n",
            "#EXTINF:-1,Channel 1\n",
            "#extvlcopt:HTTP-USER-AGENT=  Source UA/1.0  \n",
            "http://provider.example/live/user/pass/1.ts\n",
            "#EXTINF:-1,Channel 2\n",
            "#EXTVLCOPT:http-user-agent=\n",
            "http://provider.example/live/user/pass/2.ts\n",
            "#EXTVLCOPT:http-user-agent=must-not-leak\n",
            "#EXTINF:-1,Channel 3\n",
            "http://provider.example/live/user/pass/3.ts\n",
        );
        let mut items = Vec::new();

        super::consume_m3u(&Config::default(), &test_input(), make_reader(content), |item| items.push(item)).await;

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].header.source_user_agent.as_deref(), Some("Source UA/1.0"));
        assert_eq!(items[1].header.source_user_agent, None);
        assert_eq!(items[2].header.source_user_agent, None);
    }

}
