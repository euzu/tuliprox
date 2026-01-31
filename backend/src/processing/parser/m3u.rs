use crate::model::{Config, ConfigInput};
use crate::utils::request::DynReader;
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster};
use shared::utils::{default_supported_video_extensions, extract_id_from_url, Internable};
use std::borrow::BorrowMut;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use indexmap::IndexMap;
use shared::concat_string;
use crate::repository::CategoryKey;

// other implementations like calculating text_distance on all titles took too much time
// we keep it now as simple as possible and less memory intensive.
fn get_title_group(text: &Arc<str>) -> Arc<str> {
    let alphabetic_only: String = text.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    let parts = alphabetic_only.split_whitespace();
    let mut combination = String::new();
    for p in parts {
        combination = concat_string!(&combination, " " , p);
        if combination.len() > 2 {
            return combination.intern();
        }
    }
    text.clone()
}

#[inline]
fn token_value(stack: &mut String, it: &mut std::str::Chars) -> String {
    // Use .find() to skip until the first double quote (") character.
    if it.any(|ch| ch == '"') {
        // If a quote is found, call get_value to extract the value.
        return get_value(stack, it);
    }
    // If no double quote is found, return an empty string.
    String::new()
}

fn get_value(stack: &mut String, it: &mut std::str::Chars) -> String {
    for c in it.skip_while(|c| c.is_whitespace()) {
        if c == '"' {
            break;
        }
        stack.push(c);
    }

    let result = (*stack).clone();
    stack.clear();
    result
}

fn token_till(stack: &mut String, it: &mut std::str::Chars, stop_char: char, start_with_alpha: bool) -> Option<String> {
    let mut skip_non_alpha = start_with_alpha;

    for ch in it.by_ref() {
        if ch == stop_char {
            break;
        }
        if stack.is_empty() && ch.is_whitespace() {
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

    if stack.is_empty() {
        None
    } else {
        let result = (*stack).clone();
        stack.clear();
        Some(result)
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

macro_rules! process_header_fields {
    ($header:expr, $token:expr, $(($prop:ident, $field:expr)),*; $val:expr) => {
        match $token {
            $(
               $field => $header.$prop = $val,
             )*
            _ => {}
        }
    };
}

fn process_header(input_name: &Arc<str>, video_suffixes: &[String], content: &str, url: String) -> PlaylistItemHeader {
    let url_id = extract_id_from_url(&url);
    let url_types = if video_suffixes.iter().any(|suffix| url.ends_with(suffix)) {
        // TODO find Series based on group or configured names
        Some((XtreamCluster::Video, PlaylistItemType::Video))
    } else {
        None
    };

    let mut plih = create_empty_playlistitem_header(input_name, url);
    let mut it = content.chars();
    let mut stack = String::with_capacity(64);
    let line_token = token_till(&mut stack, &mut it, ':', false);
    if line_token.as_deref() == Some("#EXTINF") {
        let mut provider_id = None::<String>;
        let mut c = skip_digit(&mut it);
        loop {
            match c {
                None => break,
                Some(chr) => {
                    if chr.is_whitespace() {
                        // skip
                    } else if chr == ',' {
                        plih.title = get_value(&mut stack, &mut it).intern();
                    } else {
                        stack.push(chr);
                        let token = token_till(&mut stack, &mut it, '=', true);
                        if let Some(t) = token {
                            let value = token_value(&mut stack, &mut it);
                            let token = t.to_lowercase();
                            if token.as_str() == "xui-id" {
                                if !value.is_empty() {
                                    provider_id = Some(value);
                                }
                            } else if token == "tvg-chno" {
                                plih.chno = value.parse::<u32>().unwrap_or(0);
                            } else if token == "group-title" {
                                plih.group = value.intern();
                            } else if token == "tvg-id" {
                                plih.id = value.intern();
                            } else if token == "tvg-name" {
                                plih.name = value.intern();
                            } else if token == "tvg-logo" {
                                plih.logo = value.intern();
                            } else if token == "tvg-logo-small" {
                                plih.logo_small = value.intern();
                            } else {
                                process_header_fields!(plih, token.as_str(),
                                (parent_code, "parent-code"),
                                (audio_track, "audio-track"),
                                (time_shift, "timeshift"),
                                (rec, "tvg-rec"); value.intern());
                            }
                        }
                    }
                }
            }
            c = it.next();
        }

        if plih.id.is_empty() {
            plih.epg_channel_id = None;
            if let Some(pid) = provider_id {
                plih.id = pid.intern();
            } else if let Some(chanid) = url_id {
                plih.id = chanid.intern();
            }
        } else {
            plih.epg_channel_id = Some(Arc::clone(&plih.id));
            if let Some(pid) = provider_id {
                plih.id = pid.intern();
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
        assert_eq!(pli.id, "abc-seven".intern());
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
        assert_eq!(pli.id, "abc-seven".intern());
        assert_eq!(pli.logo, "https://abc.nz/.images/seven.png".intern());
        assert_eq!(pli.chno, 7);
        assert_eq!(&*pli.group, "Sydney");
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
}