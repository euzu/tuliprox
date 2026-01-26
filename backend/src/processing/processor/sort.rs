use crate::model::{ConfigSortRule, ConfigTarget};
use shared::foundation::ValueProvider;
use shared::model::{PlaylistGroup, SortOrder, SortTarget};
use std::cmp::Ordering;
use std::sync::Arc;

fn direction(order: SortOrder, ordering: Ordering) -> Ordering {
    match (order, ordering) {
        (SortOrder::None, _) | (_, Ordering::Equal) => Ordering::Equal,
        (SortOrder::Asc, o) => o,
        (SortOrder::Desc, o) => o.reverse(),
    }
}

fn playlist_comparator(
    sequence: Option<&Vec<Arc<regex::Regex>>>,
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
                    let o = match (va, vb) {
                        (Some(a), Some(b)) => a.cmp(b),
                        (Some(_), None) => Ordering::Greater,
                        (None, Some(_)) => Ordering::Less,
                        _ => Ordering::Equal,
                    };

                    if o != Ordering::Equal {
                        return match order {
                            SortOrder::Asc => o,
                            SortOrder::Desc => o.reverse(),
                            SortOrder::None => Ordering::Equal,
                        };
                    }
                }

                Ordering::Equal
            }
            (Some(_), None) => direction(order, Ordering::Less),
            (None, Some(_)) => direction(order, Ordering::Greater),
            (None, None) => Ordering::Equal,
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

pub(in crate::processing::processor) fn sort_playlist(
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
) -> bool {
    let Some(sort) = &target.sort else {
        return false;
    };

    let rules = &sort.rules;
    let match_as_ascii = sort.match_as_ascii;
    sort_groups(playlist, rules, match_as_ascii);
    sort_channels_in_groups(playlist, rules, match_as_ascii);

    true
}

fn sort_groups(
    groups: &mut [PlaylistGroup],
    rules: &[ConfigSortRule],
    match_as_ascii: bool,
) {
    let group_rules: Vec<_> = rules
        .iter()
        .filter(|r| matches!(r.target, SortTarget::Group))
        .filter(|r| r.order != SortOrder::None)
        .collect();

    if group_rules.is_empty() {
        return;
    }

    groups.sort_by(|a_grp, b_grp| {
        let a_chan = a_grp.channels.first();
        let b_chan = b_grp.channels.first();

        for rule in &group_rules {
            let (vp_a, vp_b) = match (a_chan, b_chan) {
                (Some(a), Some(b)) => (
                    ValueProvider { pli: a, match_as_ascii },
                    ValueProvider { pli: b, match_as_ascii },
                ),
                (Some(_), None) => return direction(rule.order, Ordering::Less),
                (None, Some(_)) => return direction(rule.order, Ordering::Greater),
                (None, None) => continue,
            };

            let fa = rule.filter.filter(&vp_a);
            let fb = rule.filter.filter(&vp_b);

            match (fa, fb) {
                (false, false) => continue,
                (true, false) => return direction(rule.order, Ordering::Less),
                (false, true) => return direction(rule.order, Ordering::Greater),
                _ => {}
            }

            let va = vp_a.get(rule.field.as_str());
            let vb = vp_b.get(rule.field.as_str());
            let ord = match (va, vb) {
                (None, None) => Ordering::Equal,
                (Some(_), None) => direction(rule.order, Ordering::Less),
                (None, Some(_)) => direction(rule.order, Ordering::Greater),
                (Some(va), Some(vb)) => {
                    playlist_comparator(rule.sequence.as_ref(), rule.order, &va, &vb)
                }
            };

            if ord != Ordering::Equal {
                return ord;
            }
        }

        Ordering::Equal
    });
}

fn sort_channels_in_groups(
    groups: &mut [PlaylistGroup],
    rules: &[ConfigSortRule],
    match_as_ascii: bool,
) {
    let channel_rules: Vec<_> = rules
        .iter()
        .filter(|r| matches!(r.target, SortTarget::Channel))
        .filter(|r| r.order != SortOrder::None)
        .collect();

    if channel_rules.is_empty() {
        return;
    }

    for group in groups {
        group.channels.sort_by(|a, b| {
            let vp_a = ValueProvider { pli: a, match_as_ascii };
            let vp_b = ValueProvider { pli: b, match_as_ascii };

            for rule in &channel_rules {
                let fa = rule.filter.filter(&vp_a);
                let fb = rule.filter.filter(&vp_b);

                match (fa, fb) {
                    (false, false) => continue,
                    (true, false) => return direction(rule.order, Ordering::Less),
                    (false, true) => return direction(rule.order, Ordering::Greater),
                    _ => {}
                }

                let va = vp_a.get(rule.field.as_str());
                let vb = vp_b.get(rule.field.as_str());

                let ord = match (&va, &vb) {
                    (None, None) => Ordering::Equal,
                    (Some(_), None) => direction(rule.order, Ordering::Less),
                    (None, Some(_)) => direction(rule.order, Ordering::Greater),
                    (Some(va), Some(vb)) => {
                        playlist_comparator(rule.sequence.as_ref(), rule.order, va, vb)
                    }
                };

                if ord == Ordering::Equal {
                    if let (Some(va), Some(vb)) = (&va, &vb) {
                        let fallback = direction(rule.order, va.cmp(vb));
                        if fallback != Ordering::Equal {
                            return fallback;
                        }
                    }
                }

                if ord != Ordering::Equal {
                    return ord;
                }
            }

            Ordering::Equal
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::model::ConfigSortRule;
    use crate::processing::processor::sort::playlist_comparator;
    use shared::foundation::Filter;
    use shared::model::{ItemField, PlaylistItem, PlaylistItemHeader, SortOrder, SortTarget};
    use std::cmp::Ordering;
    use std::sync::Arc;

    #[test]
    fn test_sort() {
        let mut channels: Vec<PlaylistItem> = vec![
            ("D", "HD"),
            ("A", "FHD"),
            ("Z", "HD"),
            ("K", "HD"),
            ("B", "HD"),
            ("A", "HD"),
            ("K", "UHD"),
            ("C", "HD"),
            ("L", "FHD"),
            ("R", "UHD"),
            ("T", "SD"),
            ("A", "FHD"),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (name, quality))| PlaylistItem {
            header: PlaylistItemHeader {
                title: format!("Chanel {name} [{quality}]").into(),
                source_ordinal: i as u32,
                ..Default::default()
            },
        })
        .collect();

        let channel_sort = ConfigSortRule {
            target: SortTarget::Channel,
            field: ItemField::Caption,
            order: SortOrder::Asc,
            sequence: Some(vec![
                shared::model::REGEX_CACHE.get_or_compile(r"(?P<c1>.*?)\bUHD\b").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"(?P<c1>.*?)\bFHD\b").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"(?P<c1>.*?)\bHD\b").unwrap(),
            ]),
            filter: Filter::default(),
        };

        channels.sort_by(|a, b| {
            let va = &a.header.title;
            let vb = &b.header.title;

            let ord =
                playlist_comparator(channel_sort.sequence.as_ref(), channel_sort.order, va, vb);

            if ord != Ordering::Equal {
                ord
            } else {
                a.header.source_ordinal.cmp(&b.header.source_ordinal)
            }
        });

        let expected = vec![
            "Chanel K [UHD]",
            "Chanel R [UHD]",
            "Chanel A [FHD]",
            "Chanel A [FHD]",
            "Chanel L [FHD]",
            "Chanel A [HD]",
            "Chanel B [HD]",
            "Chanel C [HD]",
            "Chanel D [HD]",
            "Chanel K [HD]",
            "Chanel Z [HD]",
            "Chanel T [SD]",
        ].into_iter().map(Into::into).collect::<Vec<Arc<str>>>();

        let sorted = channels
            .into_iter()
            .map(|pli| pli.header.title)
            .collect::<Vec<_>>();

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
        ]
        .into_iter()
        .enumerate()
        .map(|(i, name)| PlaylistItem {
            header: PlaylistItemHeader {
                title: name.to_string().into(),
                source_ordinal: i as u32,
                ..Default::default()
            },
        })
        .collect();

        let channel_sort = ConfigSortRule {
            target: SortTarget::Channel,
            field: ItemField::Caption,
            order: SortOrder::Asc,
            sequence: Some(vec![
                shared::model::REGEX_CACHE.get_or_compile(r"^US\| EAST.*?\[\bUHD\b\](?P<c1>.*)").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^US\| EAST.*?\[\bFHD\b\](?P<c1>.*)").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^US\| EAST.*?\[\bHD\b\](?P<c1>.*)").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^US\| EAST.*?\[\bSD\b\](?P<c1>.*)").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^US\| WEST.*?\[\bUHD\b\](?P<c1>.*)").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^US\| WEST.*?\[\bFHD\b\](?P<c1>.*)").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^US\| WEST.*?\[\bHD\b\](?P<c1>.*)").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^US\| WEST.*?\[\bSD\b\](?P<c1>.*)").unwrap(),
            ]),
            filter: Filter::default(),
        };

        channels.sort_by(|a, b| {
            let ord = playlist_comparator(
                channel_sort.sequence.as_ref(),
                channel_sort.order,
                &a.header.title,
                &b.header.title,
            );

            if ord != Ordering::Equal {
                ord
            } else {
                a.header.source_ordinal.cmp(&b.header.source_ordinal)
            }
        });

        let expected = vec![
            "US| EAST [UHD] m",
            "US| EAST [FHD] abc",
            "US| EAST [FHD] bc",
            "US| EAST [FHD] de",
            "US| EAST [FHD] def",
            "US| EAST [FHD] ghi",
            "US| EAST [HD] f",
            "US| EAST [HD] h",
            "US| EAST [HD] jkl",
            "US| EAST [HD] mno",
            "US| EAST [HD] pqrs",
            "US| EAST [HD] tuv",
            "US| EAST [HD] wxy",
            "US| EAST [HD] z",
            "US| EAST [SD] a",
            "US| EAST [SD] ijk",
            "US| EAST [SD] l",
            "US| WEST [FHD] no",
            "US| WEST [HD] qrst",
            "US| WEST [HD] uvw",
            "US| (West) xv",
            "US| East d",
            "US| West e",
            "US| West f",
        ].into_iter().map(Into::into).collect::<Vec<Arc<str>>>();

        let sorted = channels
            .into_iter()
            .map(|pli| pli.header.title)
            .collect::<Vec<_>>();

        assert_eq!(expected, sorted);
    }

}
