use crate::model::{ConfigSortChannel, ConfigSortGroup, ConfigTarget};
use deunicode::deunicode;
use std::cmp::Ordering;
use shared::foundation::filter::get_field_value;
use shared::model::{PlaylistGroup, PlaylistItem, SortOrder};

fn playlist_comparator(
    sequence: Option<&Vec<regex::Regex>>,
    order: SortOrder,
    value_a: &str,
    value_b: &str,
) -> Ordering {
    if matches!(order, SortOrder::None) {
        return Ordering::Equal;
    }
    if let Some(regex_list) = sequence {
        let mut match_a = None;
        let mut match_b = None;

        for (i, regex) in regex_list.iter().enumerate() {
            if match_a.is_none() {
                if let Some(caps) = regex.captures(value_a) {
                    match_a = Some((i, caps));
                }
            }
            if match_b.is_none() {
                if let Some(caps) = regex.captures(value_b) {
                    match_b = Some((i, caps));
                }
            }

            // If both matches found → break
            if match_a.is_some() && match_b.is_some() {
                break;
            }
        }

        match (match_a, match_b) {
            (Some((idx_a, caps_a)), Some((idx_b, caps_b))) => {
                // Different regex indices → sort by their sequence order.
                if idx_a != idx_b {
                    return match order {
                        SortOrder::Asc => idx_a.cmp(&idx_b),
                        SortOrder::Desc => idx_b.cmp(&idx_a),
                        SortOrder::None => Ordering::Equal,
                    };
                }

                // Same regex → sort by captures (c1, c2, …)
                let mut named: Vec<_> = regex_list[idx_a]
                    .capture_names()
                    .flatten()
                    .filter(|name| name.starts_with('c'))
                    .collect();

                named.sort_by_key(|name| name[1..].parse::<u32>().unwrap_or(0));

                for name in named {
                    let va = caps_a.name(name).map(|m| m.as_str());
                    let vb = caps_b.name(name).map(|m| m.as_str());
                    if let (Some(va), Some(vb)) = (va, vb) {
                        let o = va.cmp(vb);
                        if !matches!(o, Ordering::Equal) {
                            return match order {
                                SortOrder::Asc => o,
                                SortOrder::Desc => o.reverse(),
                                SortOrder::None => Ordering::Equal,
                            };
                        }
                    }
                }

                let o = value_a.cmp(value_b);
                match order {
                    SortOrder::Asc => o,
                    SortOrder::Desc => o.reverse(),
                    SortOrder::None => Ordering::Equal,
                }
            }
            (Some(_), None) => match order {
                SortOrder::Asc => Ordering::Less,
                SortOrder::Desc => Ordering::Greater,
                SortOrder::None => Ordering::Equal,
            },
            (None, Some(_)) => match order {
                SortOrder::Asc => Ordering::Greater,
                SortOrder::Desc => Ordering::Less,
                SortOrder::None => Ordering::Equal,
            },
            (None, None) => {
                // NP match → fallback
                let o = value_a.cmp(value_b);
                match order {
                    SortOrder::Asc => o,
                    SortOrder::Desc => o.reverse(),
                    SortOrder::None => Ordering::Equal,
                }
            }
        }
    } else {
        // No Regex-Sequence defined → fallback
        let o = value_a.cmp(value_b);
        match order {
            SortOrder::Asc => o,
            SortOrder::Desc => o.reverse(),
            SortOrder::None => Ordering::Equal,
        }
    }
}

fn playlistgroup_comparator(a: &PlaylistGroup, b: &PlaylistGroup, group_sort: &ConfigSortGroup, match_as_ascii: bool) -> Ordering {
    let value_a = if match_as_ascii { deunicode(&a.title) } else { a.title.to_string() };
    let value_b = if match_as_ascii { deunicode(&b.title) } else { b.title.to_string() };

    playlist_comparator(group_sort.sequence.as_ref(), group_sort.order, &value_a, &value_b)
}

fn playlistitem_comparator(
    a: &PlaylistItem,
    b: &PlaylistItem,
    channel_sort: &ConfigSortChannel,
    match_as_ascii: bool,
) -> Ordering {
    let raw_value_a = get_field_value(a, channel_sort.field);
    let raw_value_b = get_field_value(b, channel_sort.field);
    let value_a = if match_as_ascii { deunicode(&raw_value_a) } else { raw_value_a };
    let value_b = if match_as_ascii { deunicode(&raw_value_b) } else { raw_value_b };

    playlist_comparator(channel_sort.sequence.as_ref(), channel_sort.order, &value_a, &value_b)
}

pub(in crate::processing::processor) fn sort_playlist(target: &ConfigTarget, new_playlist: &mut [PlaylistGroup]) -> bool {
    if let Some(sort) = &target.sort {
        let match_as_ascii = sort.match_as_ascii;
        if let Some(group_sort) = &sort.groups {
            if !matches!(group_sort.order, SortOrder::None) {
                new_playlist.sort_by(|a, b| playlistgroup_comparator(a, b, group_sort, match_as_ascii));
            }
        }
        if let Some(channel_sorts) = &sort.channels {
            for channel_sort in channel_sorts {
                if matches!(channel_sort.order, SortOrder::None) {
                    continue;
                }
                let regexp = &channel_sort.group_pattern;
                for group in new_playlist.iter_mut() {
                    let group_title = if match_as_ascii { deunicode(&group.title) } else { group.title.to_string() };
                    if regexp.is_match(group_title.as_str()) {
                        group.channels.sort_by(|chan1, chan2| playlistitem_comparator(chan1, chan2, channel_sort, match_as_ascii));
                    }
                }
            }
        }
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::processing::processor::sort::playlistitem_comparator;
    use regex::Regex;
    use shared::model::{ItemField, PlaylistItem, PlaylistItemHeader, SortOrder};
    use crate::model::ConfigSortChannel;

    #[test]
    fn test_sort() {
        let mut channels: Vec<PlaylistItem> = vec![
            ("D", "HD"), ("A", "FHD"), ("Z", "HD"), ("K", "HD"), ("B", "HD"), ("A", "HD"),
            ("K", "UHD"), ("C", "HD"), ("L", "FHD"), ("R", "UHD"), ("T", "SD"), ("A", "FHD"),
        ].into_iter().map(|(name, quality)| PlaylistItem { header: PlaylistItemHeader { title: format!("Chanel {name} [{quality}]"), ..Default::default() } }).collect::<Vec<PlaylistItem>>();

        let channel_sort = ConfigSortChannel {
            field: ItemField::Caption,
            order: SortOrder::Asc,
            sequence: Some(vec![
                Regex::new(r"(?P<c1>.*?)\bUHD\b").unwrap(),
                Regex::new(r"(?P<c1>.*?)\bFHD\b").unwrap(),
                Regex::new(r"(?P<c1>.*?)\bHD\b").unwrap(),
            ]),
            group_pattern: Regex::new(".*").unwrap(),
        };

        channels.sort_by(|chan1, chan2| playlistitem_comparator(chan1, chan2, &channel_sort, true));
        let expected = vec!["Chanel K [UHD]", "Chanel R [UHD]", "Chanel A [FHD]", "Chanel A [FHD]", "Chanel L [FHD]", "Chanel A [HD]", "Chanel B [HD]", "Chanel C [HD]", "Chanel D [HD]", "Chanel K [HD]", "Chanel Z [HD]", "Chanel T [SD]"];
        let sorted = channels.into_iter().map(|pli| pli.header.title.clone()).collect::<Vec<String>>();
        assert_eq!(expected, sorted);
    }

    #[test]
    fn test_sort2() {
        let mut channels: Vec<PlaylistItem> = vec![
            "US| EAST [FHD] abc",
            "US| EAST [FHD] def",
            "US| EAST [FHD] ghi",
            "US| EAST [HD] jkl",
            "US| EAST [HD] mno",
            "US| EAST [HD] pqrs",
            "US| EAST [HD] tuv",
            "US| EAST [HD] wxy",
            "US| EAST [HD] z",
            "US| EAST [SD] a",
            "US| EAST [FHD] bc",
            "US| EAST [FHD] de",
            "US| EAST [HD] f",
            "US| EAST [HD] h",
            "US| EAST [SD] ijk",
            "US| EAST [SD] l",
            "US| EAST [UHD] m",
            "US| WEST [FHD] no",
            "US| WEST [HD] qrst",
            "US| WEST [HD] uvw",
            "US| (West) xv",
            "US| East d",
            "US| West e",
            "US| West f",
        ].into_iter().map(|name| PlaylistItem { header: PlaylistItemHeader { title: name.to_string(), ..Default::default() } }).collect::<Vec<PlaylistItem>>();

        let channel_sort = ConfigSortChannel {
            field: ItemField::Caption,
            order: SortOrder::Asc,
            sequence: Some(vec![
                Regex::new(r"^US\| EAST.*?\[\bUHD\b\](?P<c1>.*)").unwrap(),
                Regex::new(r"^US\| EAST.*?\[\bFHD\b\](?P<c1>.*)").unwrap(),
                Regex::new(r"^US\| EAST.*?\[\bHD\b\](?P<c1>.*)").unwrap(),
                Regex::new(r"^US\| EAST.*?\[\bSD\b\](?P<c1>.*)").unwrap(),
                Regex::new(r"^US\| WEST.*?\[\bUHD\b\](?P<c1>.*)").unwrap(),
                Regex::new(r"^US\| WEST.*?\[\bFHD\b\](?P<c1>.*)").unwrap(),
                Regex::new(r"^US\| WEST.*?\[\bHD\b\](?P<c1>.*)").unwrap(),
                Regex::new(r"^US\| WEST.*?\[\bSD\b\](?P<c1>.*)").unwrap(),
            ]),
            group_pattern: Regex::new(".*").unwrap(),
        };

        channels.sort_by(|chan1, chan2| playlistitem_comparator(chan1, chan2, &channel_sort, true));
        let sorted = channels.into_iter().map(|pli| pli.header.title.clone()).collect::<Vec<String>>();
        let expected = vec!["US| EAST [UHD] m", "US| EAST [FHD] abc", "US| EAST [FHD] bc", "US| EAST [FHD] de", "US| EAST [FHD] def", "US| EAST [FHD] ghi", "US| EAST [HD] f", "US| EAST [HD] h", "US| EAST [HD] jkl", "US| EAST [HD] mno", "US| EAST [HD] pqrs", "US| EAST [HD] tuv", "US| EAST [HD] wxy", "US| EAST [HD] z", "US| EAST [SD] a", "US| EAST [SD] ijk", "US| EAST [SD] l", "US| WEST [FHD] no", "US| WEST [HD] qrst", "US| WEST [HD] uvw", "US| (West) xv", "US| East d", "US| West e", "US| West f"];
        assert_eq!(expected, sorted);
    }
}
