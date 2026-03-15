use crate::model::{Config, ConfigInput};
use crate::utils::request::DynReader;
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster};
use shared::utils::{default_supported_video_extensions, extract_id_from_url, Internable};
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
        match it.next() {
            Some(c) => {
                if !(c == '-' || c == '+' || c.is_ascii_digit()) {
                    return Some(c);
                }
            }
            None => return None,
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
    PossibleId,
    Unknown,
}

fn classify_token(t: &str) -> M3uToken {
    if t.eq_ignore_ascii_case("xui-id") || t.eq_ignore_ascii_case("cuid") {
        M3uToken::ProviderId
    } else if t.eq_ignore_ascii_case("tvg-chno") {
        M3uToken::TvgChno
    } else if t.eq_ignore_ascii_case("group-title") {
        M3uToken::GroupTitle
    } else if t.eq_ignore_ascii_case("tvg-id") {
        M3uToken::TvgId
    } else if t.eq_ignore_ascii_case("tvg-name") {
        M3uToken::TvgName
    } else if t.eq_ignore_ascii_case("tvg-logo") {
        M3uToken::TvgLogo
    } else if t.eq_ignore_ascii_case("tvg-logo-small") {
        M3uToken::TvgLogoSmall
    } else if t.eq_ignore_ascii_case("parent-code") {
        M3uToken::ParentCode
    } else if t.eq_ignore_ascii_case("audio-track") {
        M3uToken::AudioTrack
    } else if t.eq_ignore_ascii_case("timeshift") {
        M3uToken::TimeShift
    } else if t.eq_ignore_ascii_case("tvg-rec") {
        M3uToken::TvgRec
    } else if t.eq_ignore_ascii_case("id") ||
        (t.len() > 2
        && !t.as_bytes()[..3].eq_ignore_ascii_case(b"tvg")
        && t.as_bytes()[t.len() - 2..].eq_ignore_ascii_case(b"id"))
    {
        M3uToken::PossibleId
    } else {
        M3uToken::Unknown
    }
}

fn process_header(input_name: &Arc<str>, video_suffixes: &[String], content: &str, url: String) -> PlaylistItemHeader {
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

        if let Some(pid) = provider_id {
            plih.id = pid.intern();
        } else if let Some(fid) = fallback_id {
            plih.id = fid.intern();
        } else {
            let url_id = extract_id_from_url(&plih.url);
            if !url_id.is_empty() {
                plih.id = url_id.intern();
            }
        }
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

    plih
}

pub async fn consume_m3u<F: FnMut(PlaylistItem)>(cfg: &Config, input: &ConfigInput, lines: DynReader, mut visit: F) {
    let mut header: Option<String> = None;
    let mut group: Option<String> = None;
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
        if line.starts_with("#EXTINF") {
            header = Some(line);
            continue;
        }
        if line.starts_with("#EXTGRP") {
            group = Some(String::from(&line[8..]));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(header_value) = header {
            let mut item = PlaylistItem { header: process_header(input_name, &video_suffixes, &header_value, line) };
            let header = &mut item.header;
            header.source_ordinal = ord_counter;
            ord_counter += 1;
                if header.group.is_empty() {
                    if let Some(group_value) = group {
                        header.group = group_value.intern();
                    } else {
                        let group = get_title_group(&header.title);
                        header.group = group;
                    }
                }
                visit(item);
        }
        header = None;
        group = None;
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
    use shared::utils::Internable;
    use crate::processing::parser::m3u::process_header;

    #[test]
    fn test_process_header_1() {
        let input = "hello".intern();
        let video_suffixes = Vec::new();
        let url = "http://hello.de/hello.ts";
        let line = r#"#EXTINF:-1 channel-id="abc-seven" tvg-id="abc-seven" tvg-logo="https://abc.nz/.images/seven.png" tvg-chno="7" group-title="Sydney" , Seven"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.title, "Seven".intern());
        // tvg-id is preserved as epg_channel_id, id falls back to url-derived hash
        assert_eq!(pli.epg_channel_id, Some("abc-seven".intern()));
        assert!(!pli.id.is_empty());
        assert_ne!(&*pli.id, "abc-seven"); // id should NOT be tvg-id (avoids duplicates)
        assert_eq!(pli.logo, "https://abc.nz/.images/seven.png".intern());
        assert_eq!(pli.chno, 7);
        assert_eq!(&*pli.group, "Sydney");
    }

    #[test]
    fn test_process_header_2() {
        let input = "hello".intern();
        let video_suffixes = Vec::new();
        let url = "http://hello.de/hello.ts";
        let line = r#"#EXTINF:-1 channel-id="abc-seven" tvg-id="abc-seven" tvg-logo="https://abc.nz/.images/seven.png" tvg-chno="7" group-title="Sydney", Seven"#;

        let pli = process_header(&input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.title, "Seven".intern());
        assert_eq!(pli.epg_channel_id, Some("abc-seven".intern()));
        assert!(!pli.id.is_empty());
        assert_ne!(&*pli.id, "abc-seven");
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
        assert_eq!(pli.id, "12046".intern()); // CUID is recognized as provider_id, overrides tvg-id
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
        assert_eq!(pli.id, "provider-123".intern()); // Should use xui-id
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
        assert_eq!(pli.id, "55555".intern()); // fallback numeric id field detected
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
        assert_eq!(pli.id, "77777".intern()); // CUID takes priority
    }
}
