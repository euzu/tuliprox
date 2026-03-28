use crate::model::{ConfigSortRule, ConfigTarget};
use shared::foundation::ValueProvider;
use shared::model::{PlaylistGroup, SortOrder, SortTarget};
use std::cmp::Ordering;
use std::sync::Arc;
use crate::utils::normalized_source_ordinal;

fn apply_sort_order(order: SortOrder, ordering: Ordering) -> Ordering {
    match (order, ordering) {
        (SortOrder::None, _) | (_, Ordering::Equal) => Ordering::Equal,
        (SortOrder::Asc, o) => o,
        (SortOrder::Desc, o) => o.reverse(),
    }
}

fn parse_capture_group_rank(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix('c')?;
    if suffix.is_empty() || !suffix.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u32>().ok()
}

#[derive(Debug)]
struct SequencePattern {
    regex: Arc<regex::Regex>,
    ordered_capture_names: Vec<String>,
}

#[derive(Debug)]
struct SequencePlan {
    patterns: Vec<SequencePattern>,
}

impl SequencePlan {
    fn new(sequence: &[Arc<regex::Regex>]) -> Self {
        let patterns = sequence
            .iter()
            .map(|regex| {
                let mut ordered_capture_names: Vec<(u32, String)> = regex
                    .capture_names()
                    .flatten()
                    .filter_map(|name| parse_capture_group_rank(name).map(|rank| (rank, name.to_owned())))
                    .collect();
                ordered_capture_names.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

                SequencePattern {
                    regex: Arc::clone(regex),
                    ordered_capture_names: ordered_capture_names.into_iter().map(|(_, name)| name).collect(),
                }
            })
            .collect();

        Self { patterns }
    }
}

#[derive(Debug)]
enum SequenceMatch {
    Matched { sequence_idx: usize, captures: Vec<Option<Arc<str>>> },
    Unmatched,
}

#[derive(Debug)]
struct PreparedRule<'a> {
    rule: &'a ConfigSortRule,
    sequence_plan: Option<SequencePlan>,
}

impl<'a> PreparedRule<'a> {
    fn new(rule: &'a ConfigSortRule) -> Self {
        let sequence_plan = rule.sequence.as_ref().map(|sequence| SequencePlan::new(sequence));
        Self { rule, sequence_plan }
    }
}

#[derive(Debug)]
struct RuleCacheEntry {
    filter_pass: bool,
    value: Option<Arc<str>>,
    sequence_match: Option<SequenceMatch>,
}

fn evaluate_sequence(plan: &SequencePlan, value: &str) -> SequenceMatch {
    for (sequence_idx, pattern) in plan.patterns.iter().enumerate() {
        if let Some(captures) = pattern.regex.captures(value) {
            let ordered_captures = pattern
                .ordered_capture_names
                .iter()
                .map(|name| captures.name(name).map(|capture| Arc::<str>::from(capture.as_str())))
                .collect();
            return SequenceMatch::Matched { sequence_idx, captures: ordered_captures };
        }
    }
    SequenceMatch::Unmatched
}

fn compare_sequence_match(a: &SequenceMatch, b: &SequenceMatch, order: SortOrder) -> Ordering {
    match (a, b) {
        (
            SequenceMatch::Matched { sequence_idx: idx_a, captures: captures_a },
            SequenceMatch::Matched { sequence_idx: idx_b, captures: captures_b },
        ) => {
            // Sequence index is an explicit priority list and is never reversed by DESC.
            let idx_ord = idx_a.cmp(idx_b);
            if idx_ord != Ordering::Equal {
                return idx_ord;
            }

            let capture_count = captures_a.len().max(captures_b.len());
            for index in 0..capture_count {
                let ord = match (captures_a.get(index), captures_b.get(index)) {
                    (Some(Some(v1)), Some(Some(v2))) => v1.cmp(v2),
                    (Some(Some(_)), Some(None) | None) => Ordering::Greater,
                    (Some(None) | None, Some(Some(_))) => Ordering::Less,
                    _ => Ordering::Equal,
                };
                if ord != Ordering::Equal {
                    return apply_sort_order(order, ord);
                }
            }
            Ordering::Equal
        }
        (SequenceMatch::Matched { .. }, SequenceMatch::Unmatched) => Ordering::Less,
        (SequenceMatch::Unmatched, SequenceMatch::Matched { .. }) => Ordering::Greater,
        (SequenceMatch::Unmatched, SequenceMatch::Unmatched) => Ordering::Equal,
    }
}

fn compare_rule_entries(rule: &PreparedRule, left: &RuleCacheEntry, right: &RuleCacheEntry) -> Ordering {
    match (left.filter_pass, right.filter_pass) {
        (false, false) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (true, true) => {}
    }

    match (&left.value, &right.value) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(value_left), Some(value_right)) => {
            if rule.sequence_plan.is_some() {
                match (&left.sequence_match, &right.sequence_match) {
                    (Some(seq_left), Some(seq_right)) => compare_sequence_match(seq_left, seq_right, rule.rule.order),
                    _ => Ordering::Equal,
                }
            } else {
                apply_sort_order(rule.rule.order, value_left.cmp(value_right))
            }
        }
    }
}

fn build_rule_cache_entry(
    rule: &PreparedRule,
    item: Option<&shared::model::PlaylistItem>,
    match_as_ascii: bool,
) -> RuleCacheEntry {
    let Some(item) = item else {
        return RuleCacheEntry { filter_pass: false, value: None, sequence_match: None };
    };

    let provider = ValueProvider { pli: item, match_as_ascii };
    let filter_pass = rule.rule.filter.filter(&provider);
    if !filter_pass {
        return RuleCacheEntry { filter_pass: false, value: None, sequence_match: None };
    }

    let value = provider.get(rule.rule.field.as_str());
    let sequence_match = match (&rule.sequence_plan, value.as_deref()) {
        (Some(sequence_plan), Some(value)) => Some(evaluate_sequence(sequence_plan, value)),
        _ => None,
    };

    RuleCacheEntry { filter_pass: true, value, sequence_match }
}

fn build_group_rule_cache(rule: &PreparedRule, groups: &[PlaylistGroup], match_as_ascii: bool) -> Vec<RuleCacheEntry> {
    groups.iter().map(|group| build_rule_cache_entry(rule, group.channels.first(), match_as_ascii)).collect()
}

fn build_channel_rule_cache(
    rule: &PreparedRule,
    channels: &[shared::model::PlaylistItem],
    match_as_ascii: bool,
) -> Vec<RuleCacheEntry> {
    channels.iter().map(|channel| build_rule_cache_entry(rule, Some(channel), match_as_ascii)).collect()
}

fn reorder_by_indices<T>(items: &mut Vec<T>, indices: &[usize]) {
    if items.len() != indices.len() {
        log::error!(
            "Invalid sort permutation: index length {} does not match item length {}",
            indices.len(),
            items.len()
        );
        return;
    }

    let mut seen = vec![false; items.len()];
    for &idx in indices {
        if idx >= items.len() {
            log::error!("Invalid sort permutation: index {idx} out of bounds for {} items", items.len());
            return;
        }
        if std::mem::replace(&mut seen[idx], true) {
            log::error!("Invalid sort permutation: duplicate index {idx}");
            return;
        }
    }

    let mut original: Vec<Option<T>> = std::mem::take(items).into_iter().map(Some).collect();
    items.reserve(indices.len());

    for &idx in indices {
        if let Some(item) = original[idx].take() {
            items.push(item);
        } else {
            log::error!("Invalid sort permutation: missing index {idx} after validation");
            items.extend(original.into_iter().flatten());
            return;
        }
    }
}

#[cfg(test)]
fn playlist_comparator(
    sequence: Option<&Vec<Arc<regex::Regex>>>,
    order: SortOrder,
    value_a: &str,
    value_b: &str,
) -> Ordering {
    if matches!(order, SortOrder::None) && sequence.is_none() {
        return Ordering::Equal;
    }

    if let Some(sequence) = sequence {
        let plan = SequencePlan::new(sequence);
        let left = evaluate_sequence(&plan, value_a);
        let right = evaluate_sequence(&plan, value_b);
        compare_sequence_match(&left, &right, order)
    } else {
        apply_sort_order(order, value_a.cmp(value_b))
    }
}

macro_rules! sort_groups_by_source_order {
    ($groups: ident) => {
        $groups.sort_by(|a, b| {
            let order1 = a
                .channels
                .first()
                .as_ref()
                .map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal));
            let order2 = b
                .channels
                .first()
                .as_ref()
                .map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal));
            order1.cmp(&order2)
        });
    };
}

fn compare_cached_rule_entries(
    rules: &[PreparedRule<'_>],
    rule_caches: &[Vec<RuleCacheEntry>],
    left_idx: usize,
    right_idx: usize,
) -> Ordering {
    for (rule, cache) in rules.iter().zip(rule_caches.iter()) {
        let ord = compare_rule_entries(rule, &cache[left_idx], &cache[right_idx]);
        if ord != Ordering::Equal {
            return ord;
        }
    }

    // Apply raw-value fallback only after all rule-level comparisons are exhausted.
    for (rule, cache) in rules.iter().zip(rule_caches.iter()) {
        let left = &cache[left_idx];
        let right = &cache[right_idx];
        if let (Some(va), Some(vb)) = (&left.value, &right.value) {
            let fallback = apply_sort_order(rule.rule.order, va.cmp(vb));
            if fallback != Ordering::Equal {
                return fallback;
            }
        }
    }

    Ordering::Equal
}

fn is_effective_rule(rule: &ConfigSortRule) -> bool {
    rule.order != SortOrder::None || rule.sequence.as_ref().is_some_and(|sequence| !sequence.is_empty())
}

pub(in crate::processing::processor) fn sort_playlist(
    target: &ConfigTarget,
    playlist: &mut Vec<PlaylistGroup>,
) -> bool {
    let Some(sort) = &target.sort else {
        for group in &mut *playlist {
            group
                .channels
                .sort_by_key(|a| normalized_source_ordinal(a.header.source_ordinal));
        }
        sort_groups_by_source_order!(playlist);
        return true;
    };

    let rules = &sort.rules;
    let match_as_ascii = sort.match_as_ascii;
    sort_channels_in_groups(playlist.as_mut_slice(), rules, match_as_ascii);
    sort_groups(playlist, rules, match_as_ascii);

    true
}

fn sort_groups(groups: &mut Vec<PlaylistGroup>, rules: &[ConfigSortRule], match_as_ascii: bool) {
    let group_rules: Vec<_> = rules
        .iter()
        .filter(|r| matches!(r.target, SortTarget::Group))
        .filter(|r| is_effective_rule(r))
        .map(PreparedRule::new)
        .collect();

    if group_rules.is_empty() {
        sort_groups_by_source_order!(groups);
        return;
    }

    let rule_caches: Vec<Vec<RuleCacheEntry>> =
        group_rules.iter().map(|rule| build_group_rule_cache(rule, groups.as_slice(), match_as_ascii)).collect();

    let mut group_indices: Vec<usize> = (0..groups.len()).collect();
    group_indices.sort_by(|left_idx, right_idx| {
        let ord = compare_cached_rule_entries(&group_rules, &rule_caches, *left_idx, *right_idx);
        if ord != Ordering::Equal {
            return ord;
        }

        let order1 = groups[*left_idx]
            .channels
            .first()
            .map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal));
        let order2 = groups[*right_idx]
            .channels
            .first()
            .map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal));
        order1.cmp(&order2)
    });

    reorder_by_indices(groups, &group_indices);
}

fn sort_channels_in_groups(groups: &mut [PlaylistGroup], rules: &[ConfigSortRule], match_as_ascii: bool) {
    let channel_rules: Vec<_> = rules
        .iter()
        .filter(|r| matches!(r.target, SortTarget::Channel))
        .filter(|r| is_effective_rule(r))
        .map(PreparedRule::new)
        .collect();

    if channel_rules.is_empty() {
        for group in groups {
            group
                .channels
                .sort_by_key(|a| normalized_source_ordinal(a.header.source_ordinal));
        }
        return;
    }

    for group in groups {
        let rule_caches: Vec<Vec<RuleCacheEntry>> =
            channel_rules.iter().map(|rule| build_channel_rule_cache(rule, &group.channels, match_as_ascii)).collect();

        let mut channel_indices: Vec<usize> = (0..group.channels.len()).collect();
        channel_indices.sort_by(|left_idx, right_idx| {
            let ord = compare_cached_rule_entries(&channel_rules, &rule_caches, *left_idx, *right_idx);
            if ord != Ordering::Equal {
                return ord;
            }

            normalized_source_ordinal(group.channels[*left_idx].header.source_ordinal)
                .cmp(&normalized_source_ordinal(group.channels[*right_idx].header.source_ordinal))
        });

        reorder_by_indices(&mut group.channels, &channel_indices);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compare_rule_entries, playlist_comparator, sort_channels_in_groups, sort_groups, PreparedRule, RuleCacheEntry,
    };
    use crate::model::ConfigSortRule;
    use shared::foundation::Filter;
    use shared::model::{
        ItemField, PlaylistGroup, PlaylistItem, PlaylistItemHeader, SortOrder, SortTarget, XtreamCluster,
    };
    use std::cmp::Ordering;
    use std::sync::Arc;

    fn make_group(id: u32, title: &str, channels: Vec<PlaylistItem>) -> PlaylistGroup {
        PlaylistGroup { id, title: title.into(), channels, xtream_cluster: XtreamCluster::Live }
    }

    #[test]
    fn test_sort() {
        let channels: Vec<PlaylistItem> = vec![
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
                source_ordinal: u32::try_from(i).unwrap(),
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

        let mut groups = vec![make_group(1, "G1", channels)];
        sort_channels_in_groups(groups.as_mut_slice(), &[channel_sort], false);

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
        ]
        .into_iter()
        .map(Into::into)
        .collect::<Vec<Arc<str>>>();

        let sorted = groups
            .into_iter()
            .next()
            .expect("group should exist")
            .channels
            .into_iter()
            .map(|pli| pli.header.title)
            .collect::<Vec<_>>();

        assert_eq!(expected, sorted);
    }

    #[test]
    fn test_sort2() {
        let channels: Vec<PlaylistItem> = vec![
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
                source_ordinal: u32::try_from(i).unwrap(),
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

        let mut groups = vec![make_group(1, "G1", channels)];
        sort_channels_in_groups(groups.as_mut_slice(), &[channel_sort], false);

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
        ]
        .into_iter()
        .map(Into::into)
        .collect::<Vec<Arc<str>>>();

        let sorted = groups
            .into_iter()
            .next()
            .expect("group should exist")
            .channels
            .into_iter()
            .map(|pli| pli.header.title)
            .collect::<Vec<_>>();

        assert_eq!(expected, sorted);
    }

    #[test]
    fn test_sequence_priority_is_not_reversed_for_desc() {
        let mut channels: Vec<PlaylistItem> = vec!["A-1", "B-9", "A-7", "B-2"]
            .into_iter()
            .enumerate()
            .map(|(i, title)| PlaylistItem {
                header: PlaylistItemHeader {
                    title: title.to_string().into(),
                    source_ordinal: u32::try_from(i).unwrap(),
                    ..Default::default()
                },
            })
            .collect();

        let sequence = vec![
            shared::model::REGEX_CACHE.get_or_compile(r"^A-(?P<c1>\d+)$").unwrap(),
            shared::model::REGEX_CACHE.get_or_compile(r"^B-(?P<c1>\d+)$").unwrap(),
        ];

        channels.sort_by(|a, b| {
            let ord = playlist_comparator(Some(&sequence), SortOrder::Desc, &a.header.title, &b.header.title);

            if ord == Ordering::Equal {
                a.header.source_ordinal.cmp(&b.header.source_ordinal)
            } else {
                ord
            }
        });

        let sorted = channels.into_iter().map(|pli| pli.header.title).collect::<Vec<_>>();

        let expected = vec!["A-7", "A-1", "B-9", "B-2"].into_iter().map(Into::into).collect::<Vec<Arc<str>>>();

        assert_eq!(expected, sorted);
    }

    #[test]
    fn test_compare_rule_entries_desc_filter_mismatch() {
        let rule = ConfigSortRule {
            target: SortTarget::Channel,
            field: ItemField::Caption,
            order: SortOrder::Desc,
            sequence: Some(vec![shared::model::REGEX_CACHE.get_or_compile(r"^A-(?P<c1>\d+)$").unwrap()]),
            filter: Filter::default(),
        };

        let prepared = PreparedRule::new(&rule);
        let pass_with_value = RuleCacheEntry { filter_pass: true, value: Some(Arc::from("A-2")), sequence_match: None };
        let pass_without_value = RuleCacheEntry { filter_pass: true, value: None, sequence_match: None };
        let fail_filter = RuleCacheEntry { filter_pass: false, value: None, sequence_match: None };

        assert_eq!(compare_rule_entries(&prepared, &pass_with_value, &fail_filter), Ordering::Less);
        assert_eq!(compare_rule_entries(&prepared, &fail_filter, &pass_with_value), Ordering::Greater);
        assert_eq!(compare_rule_entries(&prepared, &pass_with_value, &pass_without_value), Ordering::Less);
        assert_eq!(compare_rule_entries(&prepared, &pass_without_value, &pass_with_value), Ordering::Greater);
    }

    #[test]
    fn test_sort_channels_does_not_short_circuit_later_rules_with_raw_value_fallback() {
        let channels: Vec<PlaylistItem> = vec!["A-2-x", "A-1-y"]
            .into_iter()
            .enumerate()
            .map(|(i, title)| PlaylistItem {
                header: PlaylistItemHeader {
                    title: title.to_string().into(),
                    source_ordinal: u32::try_from(i).unwrap(),
                    ..Default::default()
                },
            })
            .collect();

        let first_rule = ConfigSortRule {
            target: SortTarget::Channel,
            field: ItemField::Caption,
            order: SortOrder::Asc,
            // Both values match the same sequence item and produce equal sequence priority.
            sequence: Some(vec![shared::model::REGEX_CACHE.get_or_compile(r"^A-\d+-.$").unwrap()]),
            filter: Filter::default(),
        };
        let second_rule = ConfigSortRule {
            target: SortTarget::Channel,
            field: ItemField::Caption,
            order: SortOrder::Desc,
            sequence: None,
            filter: Filter::default(),
        };

        let mut groups = vec![make_group(1, "G1", channels)];
        sort_channels_in_groups(groups.as_mut_slice(), &[first_rule, second_rule], false);
        let sorted = groups[0].channels.iter().map(|pli| pli.header.title.clone()).collect::<Vec<_>>();

        let expected = vec!["A-2-x", "A-1-y"].into_iter().map(Into::into).collect::<Vec<Arc<str>>>();
        assert_eq!(expected, sorted);
    }

    #[test]
    fn test_sort_groups_uses_raw_value_fallback_before_source_ordinal() {
        let group_a = make_group(
            1,
            "A",
            vec![PlaylistItem {
                header: PlaylistItemHeader { title: Arc::from("A-2-x"), source_ordinal: 0, ..Default::default() },
            }],
        );
        let group_b = make_group(
            2,
            "B",
            vec![PlaylistItem {
                header: PlaylistItemHeader { title: Arc::from("A-1-y"), source_ordinal: 1, ..Default::default() },
            }],
        );

        let group_rule = ConfigSortRule {
            target: SortTarget::Group,
            field: ItemField::Caption,
            order: SortOrder::Asc,
            // Both groups match the same sequence item and produce equal sequence priority.
            sequence: Some(vec![shared::model::REGEX_CACHE.get_or_compile(r"^A-\d+-.$").unwrap()]),
            filter: Filter::default(),
        };

        let mut groups = vec![group_a, group_b];
        sort_groups(&mut groups, &[group_rule], false);

        let sorted = groups.iter().map(|group| group.channels[0].header.title.clone()).collect::<Vec<_>>();
        let expected = vec!["A-1-y", "A-2-x"].into_iter().map(Into::into).collect::<Vec<Arc<str>>>();
        assert_eq!(expected, sorted);
    }

    #[test]
    fn test_sort_groups_normalized_source_ordinal_tiebreaker_pushes_zero_last() {
        let group_zero = make_group(
            1,
            "zero",
            vec![PlaylistItem {
                header: PlaylistItemHeader {
                    title: Arc::from("Same Caption"),
                    source_ordinal: 0,
                    ..Default::default()
                },
            }],
        );
        let group_non_zero = make_group(
            2,
            "non-zero",
            vec![PlaylistItem {
                header: PlaylistItemHeader {
                    title: Arc::from("Same Caption"),
                    source_ordinal: 7,
                    ..Default::default()
                },
            }],
        );

        let group_rule = ConfigSortRule {
            target: SortTarget::Group,
            field: ItemField::Caption,
            order: SortOrder::Asc,
            // Both groups have the same sequence/rule priority and same caption value.
            sequence: Some(vec![shared::model::REGEX_CACHE.get_or_compile(r"^Same Caption$").unwrap()]),
            filter: Filter::default(),
        };

        let mut groups = vec![group_zero, group_non_zero];
        sort_groups(&mut groups, &[group_rule], false);

        assert_eq!(groups[0].title.as_ref(), "non-zero");
        assert_eq!(groups[0].channels[0].header.source_ordinal, 7);
        assert_eq!(groups[1].title.as_ref(), "zero");
        assert_eq!(groups[1].channels[0].header.source_ordinal, 0);
    }

    #[test]
    fn test_channel_sequence_applies_even_when_order_is_none() {
        let channels: Vec<PlaylistItem> = vec!["HD", "UHD", "FHD", "SD"]
            .into_iter()
            .enumerate()
            .map(|(i, title)| PlaylistItem {
                header: PlaylistItemHeader {
                    title: title.to_string().into(),
                    source_ordinal: u32::try_from(i).unwrap(),
                    ..Default::default()
                },
            })
            .collect();

        let channel_sort = ConfigSortRule {
            target: SortTarget::Channel,
            field: ItemField::Caption,
            order: SortOrder::None,
            sequence: Some(vec![
                shared::model::REGEX_CACHE.get_or_compile(r"^UHD$").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^FHD$").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^HD$").unwrap(),
            ]),
            filter: Filter::default(),
        };

        let mut groups = vec![make_group(1, "G1", channels)];
        sort_channels_in_groups(groups.as_mut_slice(), &[channel_sort], false);

        let sorted = groups[0].channels.iter().map(|pli| pli.header.title.clone()).collect::<Vec<_>>();
        let expected = vec!["UHD", "FHD", "HD", "SD"].into_iter().map(Into::into).collect::<Vec<Arc<str>>>();
        assert_eq!(expected, sorted);
    }

    #[test]
    fn test_group_sequence_applies_even_when_order_is_none() {
        let group_hd = make_group(
            1,
            "hd",
            vec![PlaylistItem {
                header: PlaylistItemHeader { title: Arc::from("HD"), source_ordinal: 2, ..Default::default() },
            }],
        );
        let group_uhd = make_group(
            2,
            "uhd",
            vec![PlaylistItem {
                header: PlaylistItemHeader { title: Arc::from("UHD"), source_ordinal: 3, ..Default::default() },
            }],
        );
        let group_fhd = make_group(
            3,
            "fhd",
            vec![PlaylistItem {
                header: PlaylistItemHeader { title: Arc::from("FHD"), source_ordinal: 1, ..Default::default() },
            }],
        );

        let group_sort = ConfigSortRule {
            target: SortTarget::Group,
            field: ItemField::Caption,
            order: SortOrder::None,
            sequence: Some(vec![
                shared::model::REGEX_CACHE.get_or_compile(r"^UHD$").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^FHD$").unwrap(),
                shared::model::REGEX_CACHE.get_or_compile(r"^HD$").unwrap(),
            ]),
            filter: Filter::default(),
        };

        let mut groups = vec![group_hd, group_uhd, group_fhd];
        sort_groups(&mut groups, &[group_sort], false);

        let sorted = groups.iter().map(|group| group.channels[0].header.title.clone()).collect::<Vec<_>>();
        let expected = vec!["UHD", "FHD", "HD"].into_iter().map(Into::into).collect::<Vec<Arc<str>>>();
        assert_eq!(expected, sorted);
    }

    #[test]
    fn test_empty_sequence_with_none_order_is_ignored() {
        let channels: Vec<PlaylistItem> = vec!["B", "A"]
            .into_iter()
            .enumerate()
            .map(|(i, title)| PlaylistItem {
                header: PlaylistItemHeader {
                    title: title.to_string().into(),
                    source_ordinal: u32::try_from(i + 1).unwrap(),
                    ..Default::default()
                },
            })
            .collect();

        let channel_sort = ConfigSortRule {
            target: SortTarget::Channel,
            field: ItemField::Caption,
            order: SortOrder::None,
            sequence: Some(vec![]),
            filter: Filter::default(),
        };

        let mut groups = vec![make_group(1, "G1", channels)];
        sort_channels_in_groups(groups.as_mut_slice(), &[channel_sort], false);

        let sorted = groups[0].channels.iter().map(|pli| pli.header.title.clone()).collect::<Vec<_>>();
        let expected = vec!["B", "A"].into_iter().map(Into::into).collect::<Vec<Arc<str>>>();
        assert_eq!(expected, sorted);
    }
}
