use crate::model::{Config, ConfigInput};
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster};
use shared::utils::extract_id_from_url;
use std::borrow::BorrowMut;


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

    let result = (*stack).to_string();
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
        let result = (*stack).to_string();
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

fn create_empty_playlistitem_header(input_name: &str, url: &str) -> PlaylistItemHeader {
    PlaylistItemHeader {
        url: url.to_owned(),
        category_id: 0,
        input_name: input_name.to_string(),
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

fn process_header(input_name: &str, video_suffixes: &[&str], content: &str, url: &str) -> PlaylistItemHeader {
    let mut plih = create_empty_playlistitem_header(input_name, url);
    let mut it = content.chars();
    let mut stack = String::with_capacity(64);
    let line_token = token_till(&mut stack, &mut it, ':', false);
    if line_token.as_deref() == Some("#EXTINF") {
        let mut provider_id = None::<String>;
        let mut c = skip_digit(&mut it);
        loop {
            if c.is_none() {
                break;
            }
            let chr = c.unwrap();
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
                    } else {
                        process_header_fields!(plih, token.as_str(),
                            (id, "tvg-id"),
                            (group, "group-title"),
                            (name, "tvg-name"),
                            (chno, "tvg-chno"),
                            (parent_code, "parent-code"),
                            (audio_track, "audio-track"),
                            (logo, "tvg-logo"),
                            (logo_small, "tvg-logo-small"),
                            (time_shift, "timeshift"),
                            (rec, "tvg-rec"); value);
                    }
                }
            }
            c = it.next();
        }

        if plih.id.is_empty() {
            plih.epg_channel_id = None;
            if let Some(pid) = provider_id {
                plih.id = pid;
            } else if let Some(chanid) = extract_id_from_url(url) {
                plih.id = chanid;
            }
        } else {
            plih.epg_channel_id = Some(plih.id.to_string());
            if let Some(pid) = provider_id {
                plih.id = pid;
            }
        }
    }

    if video_suffixes.iter().any(|suffix| url.ends_with(suffix)) {
        // TODO find Series based on group or configured names
        plih.xtream_cluster = XtreamCluster::Video;
        plih.item_type = PlaylistItemType::Video;
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


pub fn consume_m3u<'a, I, F: FnMut(PlaylistItem)>(cfg: &Config, input: &ConfigInput, lines: I, mut visit: F)
where
    I: Iterator<Item=&'a str>,
{
    let mut header: Option<String> = None;
    let mut group: Option<String> = None;
    let input_name = input.name.as_str();

    let video_suffixes = cfg.video.as_ref().unwrap().extensions.iter().map(String::as_str).collect::<Vec<&str>>();
    for line in lines {
        if line.starts_with("#EXTINF") {
            header = Some(String::from(line));
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
            if header.group.is_empty() {
                if let Some(group_value) = group {
                    header.group = group_value;
                } else {
                    let current_title = header.title.clone();
                    header.group = get_title_group(current_title.as_str());
                }
            }
            visit(item);
        }
        header = None;
        group = None;
    }
}

pub fn parse_m3u<'a, I>(cfg: &Config, input: &ConfigInput, lines: I) -> Vec<PlaylistGroup>
where
    I: Iterator<Item=&'a str>,
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
                sort_order.get_mut(*o.get()).unwrap().push(item);
            }
        }
    });
    let mut grp_id = 0;
    let result: Vec<PlaylistGroup> = sort_order.into_iter().map(|channels| {
        // create a group based on the first playlist item
        let channel = channels.first();
        let (cluster, group_title) = channel.map(|pli|
            (pli.header.xtream_cluster, &pli.header.group)).unwrap();
        grp_id += 1;
        PlaylistGroup { id: grp_id, xtream_cluster: cluster, title: group_title.to_string(), channels }
    }).collect();
    result
}

#[cfg(test)]
mod test {
    use crate::processing::parser::m3u::process_header;

    #[test]
    fn test_process_header_1() {
        let input: &str = "hello";
        let video_suffixes = Vec::new();
        let url = "http://hello.de/hello.ts";
        let line = r#"#EXTINF:-1 channel-id="abc-seven" tvg-id="abc-seven" tvg-logo="https://abc.nz/.images/seven.png" tvg-chno="7" group-title="Sydney" , Seven"#;

        let pli = process_header(input, &video_suffixes, line, url);
        assert_eq!(pli.title, "Seven");
        assert_eq!(pli.id, "abc-seven");
        assert_eq!(pli.logo, "https://abc.nz/.images/seven.png");
        assert_eq!(pli.chno, "7");
        assert_eq!(pli.group, "Sydney");
    }

    #[test]
    fn test_process_header_2() {
        let input: &str = "hello";
        let video_suffixes = Vec::new();
        let url = "http://hello.de/hello.ts";
        let line = r#"#EXTINF:-1 channel-id="abc-seven" tvg-id="abc-seven" tvg-logo="https://abc.nz/.images/seven.png" tvg-chno="7" group-title="Sydney", Seven"#;

        let pli = process_header(input, &video_suffixes, line, url);
        assert_eq!(pli.title, "Seven");
        assert_eq!(pli.id, "abc-seven");
        assert_eq!(pli.logo, "https://abc.nz/.images/seven.png");
        assert_eq!(pli.chno, "7");
        assert_eq!(pli.group, "Sydney");
    }
}