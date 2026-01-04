use crate::model::{Config, ConfigInput};
use crate::utils::request::DynReader;
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster};
use shared::utils::{default_supported_video_extensions, extract_id_from_url, StringInterner};
use std::borrow::BorrowMut;
use tokio::io::AsyncBufReadExt;

// other implementations like calculating text_distance on all titles took too much time
// we keep it now as simple as possible and less memory intensive.
fn get_title_group(text: &str) -> String {
    let alphabetic_only: String = text.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    let parts = alphabetic_only.split_whitespace();
    let mut combination = String::new();
    for p in parts {
        combination = format!("{combination} {p}").trim().to_string();
        if combination.len() > 2 {
            return combination;
        }
    }
    text.to_string()
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

fn create_empty_playlistitem_header(interner: &mut StringInterner, input_name: &str, url: String) -> PlaylistItemHeader {
    PlaylistItemHeader {
        url,
        category_id: 0,
        input_name: interner.intern(input_name),
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

fn process_header(interner: &mut StringInterner, input_name: &str, video_suffixes: &[String], content: &str, url: String) -> PlaylistItemHeader {
    let url_id = extract_id_from_url(&url);
    let url_types = if video_suffixes.iter().any(|suffix| url.ends_with(suffix)) {
        // TODO find Series based on group or configured names
        Some((XtreamCluster::Video, PlaylistItemType::Video))
    } else {
        None
    };

    let mut plih = create_empty_playlistitem_header(interner, input_name, url);
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
                        plih.title = get_value(&mut stack, &mut it);
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
                            } else {
                                process_header_fields!(plih, token.as_str(),
                                (id, "tvg-id"),
                                (name, "tvg-name"),
                                (parent_code, "parent-code"),
                                (audio_track, "audio-track"),
                                (logo, "tvg-logo"),
                                (logo_small, "tvg-logo-small"),
                                (time_shift, "timeshift"),
                                (rec, "tvg-rec"); value.clone());
                                if token == "group-title" {
                                    plih.group = interner.intern(&value);
                                }
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
                plih.id = pid;
            } else if let Some(chanid) = url_id {
                plih.id = chanid;
            }
        } else {
            plih.epg_channel_id = Some(plih.id.clone());
            if let Some(pid) = provider_id {
                plih.id = pid;
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
    let mut interner = StringInterner::new();
    let input_name = input.name.as_str();

    let video_suffixes = match cfg.video.as_ref() {
        Some(config) => {
            config.extensions.iter().map(Clone::clone).collect::<Vec<String>>()
        }
        None => default_supported_video_extensions()
    };
    let mut lines = tokio::io::BufReader::new(lines).lines();
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
            let mut item = PlaylistItem { header: process_header(&mut interner, input_name, &video_suffixes, &header_value, line) };
            let header = &mut item.header;
            if header.group.is_empty() {
                if let Some(group_value) = group {
                    header.group = interner.intern(&group_value);
                } else {
                    let current_title = header.title.clone();
                    header.group = interner.intern(&get_title_group(current_title.as_str()));
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
    let mut sort_order: Vec<Vec<PlaylistItem>> = vec![];
    let mut sort_order_idx: usize = 0;
    let mut group_map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    consume_m3u(cfg, input, lines, |item| {
        // keep the original sort order for groups and group the playlist items
        let key = {
            let header = &item.header;
            format!("{}{}", &header.xtream_cluster, &header.group)
        };
        match group_map.entry(key) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(sort_order_idx);
                sort_order.push(vec![item]);
                sort_order_idx += 1;
            }
            std::collections::hash_map::Entry::Occupied(o) => {
                if let Some(order) = sort_order.get_mut(*o.get()) {
                    order.push(item);
                }
            }
        }
    }).await;
    let mut grp_id = 0;
    let result: Vec<PlaylistGroup> = sort_order.into_iter().filter_map(|channels| {
        // create a group based on the first playlist item
        let channel = channels.first();
        if let Some((cluster, group_title)) = channel.map(|pli|
            (pli.header.xtream_cluster, &pli.header.group)) {
            grp_id += 1;
            Some(PlaylistGroup { id: grp_id, xtream_cluster: cluster, title: group_title.to_string(), channels })
        } else {
            None
        }
    }).collect();
    result
}

#[cfg(test)]
mod test {
    use crate::processing::parser::m3u::process_header;
    use shared::utils::StringInterner;

    #[test]
    fn test_process_header_1() {
        let mut interner = StringInterner::new();
        let input: &str = "hello";
        let video_suffixes = Vec::new();
        let url = "http://hello.de/hello.ts";
        let line = r#"#EXTINF:-1 channel-id="abc-seven" tvg-id="abc-seven" tvg-logo="https://abc.nz/.images/seven.png" tvg-chno="7" group-title="Sydney" , Seven"#;

        let pli = process_header(&mut interner, input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.title, "Seven");
        assert_eq!(pli.id, "abc-seven");
        assert_eq!(pli.logo, "https://abc.nz/.images/seven.png");
        assert_eq!(pli.chno, 7);
        assert_eq!(&*pli.group, "Sydney");
    }

    #[test]
    fn test_process_header_2() {
        let mut interner = StringInterner::new();
        let input: &str = "hello";
        let video_suffixes = Vec::new();
        let url = "http://hello.de/hello.ts";
        let line = r#"#EXTINF:-1 channel-id="abc-seven" tvg-id="abc-seven" tvg-logo="https://abc.nz/.images/seven.png" tvg-chno="7" group-title="Sydney", Seven"#;

        let pli = process_header(&mut interner, input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.title, "Seven");
        assert_eq!(pli.id, "abc-seven");
        assert_eq!(pli.logo, "https://abc.nz/.images/seven.png");
        assert_eq!(pli.chno, 7);
        assert_eq!(&*pli.group, "Sydney");
    }

    #[test]
    fn test_process_header_xui_id() {
        let mut interner = StringInterner::new();
        let input: &str = "hello";
        let video_suffixes = Vec::new();
        let url = "http://hello.de/hello.ts";
        let line = r#"#EXTINF:-1 tvg-id="abc-seven" xui-id="provider-123" group-title="Sydney", Seven"#;

        let pli = process_header(&mut interner, input, &video_suffixes, line, url.to_string());
        assert_eq!(pli.title, "Seven");
        assert_eq!(pli.id, "provider-123"); // Should use xui-id
        assert_eq!(pli.epg_channel_id, Some("abc-seven".to_string())); // Should preserve original tvg-id
        assert_eq!(&*pli.group, "Sydney");
    }
}