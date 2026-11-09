use super::match_key::{dedup_match_by, extract_normalized_match_key, raw_match_value};
use indexmap::IndexMap;
use shared::{
    model::{
        DeduplicateConfig, DeduplicateKeep, PlaylistGroup, PlaylistItem, PluginDuplicateGroup, PluginKeepers, UUIDType,
        XtreamCluster,
    },
    utils::quality_rank,
};
use std::collections::HashSet;

fn match_value(config: &DeduplicateConfig, item: &PlaylistItem) -> String {
    extract_normalized_match_key(&item.header, dedup_match_by(config.match_by), config.match_as_ascii, None)
}

fn item_raw_match_value<'a>(config: &DeduplicateConfig, item: &'a PlaylistItem) -> &'a str {
    raw_match_value(&item.header, dedup_match_by(config.match_by))
}

/// Failover group items and their members never take part in deduplication.
fn should_skip(channel: &PlaylistItem, skip: &HashSet<UUIDType>) -> bool {
    skip.contains(&channel.header.uuid) || channel.header.failover_group_uuid.is_some()
}

/// Channels sharing one `(cluster, normalized key)`: the duplicates of each
/// other. `members` are `(group index, channel index)` in playlist order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::processor) struct DuplicateSet {
    pub(in crate::processor) key: String,
    pub(in crate::processor) members: Vec<(usize, usize)>,
}

/// The sets of two or more duplicates in `playlist`, ordered by their first
/// member. Pure: nothing is removed. Empty match keys never form a set.
pub(in crate::processor) fn group_duplicates(
    config: &DeduplicateConfig,
    playlist: &[PlaylistGroup],
    skip: &HashSet<UUIDType>,
) -> Vec<DuplicateSet> {
    let mut sets: IndexMap<(XtreamCluster, String), Vec<(usize, usize)>> = IndexMap::new();
    for (group_idx, group) in playlist.iter().enumerate() {
        for (channel_idx, channel) in group.channels.iter().enumerate() {
            if should_skip(channel, skip) {
                continue;
            }
            let key_value = match_value(config, channel);
            if key_value.is_empty() {
                continue;
            }
            sets.entry((group.xtream_cluster, key_value)).or_default().push((group_idx, channel_idx));
        }
    }
    sets.into_iter()
        .filter(|(_, members)| members.len() > 1)
        .map(|((_, key), members)| DuplicateSet { key, members })
        .collect()
}

/// The sets as a `resolve-duplicates` plugin sees them, in the same order.
pub(in crate::processor) fn plugin_duplicate_groups<'a>(
    playlist: &'a [PlaylistGroup],
    sets: &'a [DuplicateSet],
) -> Vec<PluginDuplicateGroup<'a>> {
    sets.iter()
        .map(|set| PluginDuplicateGroup {
            key: &set.key,
            members: set.members.iter().map(|&(group, channel)| &playlist[group].channels[channel]).collect(),
        })
        .collect()
}

/// The member index the host keeps: the first of the best quality, or the
/// first. `Plugin` is decided by the plugin; what it leaves open is best quality.
fn host_winner(config: &DeduplicateConfig, playlist: &[PlaylistGroup], set: &DuplicateSet) -> usize {
    if config.keep == DeduplicateKeep::First {
        return 0;
    }
    let mut winner = (0usize, 0u8);
    for (index, &(group, channel)) in set.members.iter().enumerate() {
        let rank = quality_rank(item_raw_match_value(config, &playlist[group].channels[channel]));
        // strictly greater: ties keep the first occurrence
        if index == 0 || rank > winner.1 {
            winner = (index, rank);
        }
    }
    winner.0
}

/// The members a plugin asked to keep, if it named at least one valid one.
fn plugin_keepers(keepers: &[usize], members: usize) -> Option<Vec<usize>> {
    let mut valid: Vec<usize> = keepers.iter().copied().filter(|index| *index < members).collect();
    valid.sort_unstable();
    valid.dedup();
    (!valid.is_empty()).then_some(valid)
}

/// Collapse duplicate channels across the whole target playlist (per cluster).
/// Returns the number of removed channels. Empty match keys are never
/// deduplicated; ties keep the first occurrence in playlist order.
///
/// `keepers` is the `resolve-duplicates` decision, prepared by the async
/// caller from [`group_duplicates`] of this same playlist: per set, in set
/// order, the member indices to keep. An empty entry, an entry naming no valid
/// member, or a list of the wrong length falls back to `config.keep`
/// (`BestQuality` for a plugin).
pub(in crate::processor) fn deduplicate_playlist(
    config: &DeduplicateConfig,
    playlist: &mut Vec<PlaylistGroup>,
    keepers: Option<&PluginKeepers>,
    skip: &HashSet<UUIDType>,
) -> usize {
    let sets = group_duplicates(config, playlist, skip);
    let keepers = keepers.filter(|keepers| {
        let aligned = keepers.len() == sets.len();
        if !aligned {
            log::warn!(
                "resolve-duplicates answered {} groups for {} duplicate groups; using the host's keep",
                keepers.len(),
                sets.len()
            );
        }
        aligned
    });

    let mut losers: HashSet<(usize, usize)> = HashSet::new();
    for (index, set) in sets.iter().enumerate() {
        let kept = keepers
            .and_then(|keepers| plugin_keepers(&keepers[index], set.members.len()))
            .unwrap_or_else(|| vec![host_winner(config, playlist, set)]);
        losers.extend(set.members.iter().enumerate().filter(|(member, _)| !kept.contains(member)).map(|(_, at)| *at));
    }

    let mut removed = 0;
    // Only drop groups that deduplication emptied; groups that were already empty stay untouched.
    let mut emptied_by_dedup = vec![false; playlist.len()];
    for (group_idx, group) in playlist.iter_mut().enumerate() {
        let before = group.channels.len();
        let mut channel_idx = 0usize;
        group.channels.retain(|_| {
            let idx = channel_idx;
            channel_idx += 1;
            !losers.contains(&(group_idx, idx))
        });
        removed += before - group.channels.len();
        emptied_by_dedup[group_idx] = before > 0 && group.channels.is_empty();
    }
    let mut group_idx = 0usize;
    playlist.retain(|_| {
        let keep = !emptied_by_dedup[group_idx];
        group_idx += 1;
        keep
    });
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{
        model::{DeduplicateMatchBy, PlaylistItemHeader},
        utils::Internable,
    };

    fn make_item(title: &str) -> PlaylistItem {
        PlaylistItem { header: PlaylistItemHeader { title: title.intern(), ..Default::default() } }
    }

    fn make_group(title: &str, channels: Vec<PlaylistItem>) -> PlaylistGroup {
        PlaylistGroup { id: 1, title: title.intern(), channels, xtream_cluster: XtreamCluster::Live }
    }

    #[test]
    fn quality_rank_recognizes_tokens() {
        assert_eq!(quality_rank("News [UHD]"), 5);
        assert_eq!(quality_rank("News 1080p"), 3);
        assert_eq!(quality_rank("News HD"), 2);
        assert_eq!(quality_rank("News"), 0);
        assert_eq!(quality_rank("HDTV News"), 0); // no partial token match
    }

    #[test]
    fn dedup_keeps_best_quality() {
        let mut playlist = vec![make_group(
            "G",
            vec![make_item("News HD"), make_item("News [FHD]"), make_item("News"), make_item("Sports HD")],
        )];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Caption,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 2);
        let titles: Vec<_> = playlist[0].channels.iter().map(|c| c.header.title.to_string()).collect();
        assert_eq!(titles, vec!["News [FHD]", "Sports HD"]);
    }

    #[test]
    fn post_merge_dedup_selects_the_eligible_winner_before_curation_projection() {
        let mut playlist =
            vec![make_group("Base", vec![make_item("Movie HD")]), make_group("Trending", vec![make_item("Movie 4K")])];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Caption,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };

        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());

        // Target-wide curation evaluates only this surviving winner.
        assert_eq!(removed, 1);
        assert_eq!(playlist.len(), 1);
        assert_eq!(playlist[0].title.as_ref(), "Trending");
        assert_eq!(playlist[0].channels[0].header.title.as_ref(), "Movie 4K");
    }

    #[test]
    fn dedup_keep_first_preserves_playlist_order_winner() {
        let mut playlist = vec![make_group("G", vec![make_item("News HD"), make_item("News [FHD]")])];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Caption,
            keep: DeduplicateKeep::First,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 1);
        assert_eq!(playlist[0].channels[0].header.title.as_ref(), "News HD");
    }

    #[test]
    fn dedup_ignores_empty_keys_and_drops_empty_groups() {
        let mut playlist = vec![
            make_group("Empty", vec![]),
            make_group("A", vec![make_item("News HD")]),
            make_group("B", vec![make_item("News FHD"), make_item("")]),
        ];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Caption,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 1);
        // "A" was emptied by dedup and dropped, the already-empty group survives
        assert_eq!(playlist.len(), 2);
        assert_eq!(playlist[0].title.as_ref(), "Empty");
        assert_eq!(playlist[1].title.as_ref(), "B");
        // the empty-caption item has no dedup key and is retained
        let titles: Vec<_> = playlist[1].channels.iter().map(|c| c.header.title.to_string()).collect();
        assert_eq!(titles, vec!["News FHD", ""]);
    }

    #[test]
    fn dedup_match_as_ascii_collapses_accented_names() {
        let mut playlist = vec![make_group("G", vec![make_item("Café HD"), make_item("Cafe FHD")])];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Caption,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: true,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 1);
        assert_eq!(playlist[0].channels[0].header.title.as_ref(), "Cafe FHD");

        // without the flag the accented name stays distinct
        let mut playlist = vec![make_group("G", vec![make_item("Café HD"), make_item("Cafe FHD")])];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Caption,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        assert_eq!(deduplicate_playlist(&config, &mut playlist, None, &HashSet::new()), 0);
    }

    fn plugin_config() -> DeduplicateConfig {
        DeduplicateConfig {
            match_by: DeduplicateMatchBy::Caption,
            keep: DeduplicateKeep::Plugin("plugin:picker".parse().unwrap()),
            match_as_ascii: false,
        }
    }

    #[test]
    fn group_duplicates_is_pure_and_only_returns_sets_of_two_or_more() {
        let playlist = vec![
            make_group("A", vec![make_item("News HD"), make_item("Sports"), make_item("")]),
            make_group("B", vec![make_item("News [FHD]"), make_item("")]),
        ];
        let sets = group_duplicates(&plugin_config(), &playlist, &HashSet::new());
        assert_eq!(sets, vec![DuplicateSet { key: "news".to_string(), members: vec![(0, 0), (1, 0)] }]);
        assert_eq!(playlist[0].channels.len(), 3, "grouping removes nothing");
        let groups = plugin_duplicate_groups(&playlist, &sets);
        assert_eq!((groups[0].key, groups[0].members.len()), ("news", 2));
    }

    #[test]
    fn plugin_keepers_override_the_host_choice() {
        let mut playlist = vec![make_group("G", vec![make_item("News HD"), make_item("News [FHD]")])];
        // the host would keep the FHD one (index 1); the plugin keeps the first
        let keepers: PluginKeepers = vec![vec![0]];
        assert_eq!(deduplicate_playlist(&plugin_config(), &mut playlist, Some(&keepers), &HashSet::new()), 1);
        assert_eq!(playlist[0].channels[0].header.title.as_ref(), "News HD");
    }

    #[test]
    fn empty_or_invalid_plugin_keepers_fall_back_to_the_host_keep() {
        for keepers in [vec![vec![]], vec![vec![7]], vec![vec![0], vec![0]]] {
            let mut playlist = vec![make_group("G", vec![make_item("News HD"), make_item("News [FHD]")])];
            assert_eq!(deduplicate_playlist(&plugin_config(), &mut playlist, Some(&keepers), &HashSet::new()), 1);
            assert_eq!(playlist[0].channels[0].header.title.as_ref(), "News [FHD]");
        }
    }

    #[test]
    fn a_plugin_may_keep_several_members() {
        let mut playlist = vec![make_group("G", vec![make_item("News HD"), make_item("News"), make_item("News 4K")])];
        let keepers: PluginKeepers = vec![vec![2, 0]];
        assert_eq!(deduplicate_playlist(&plugin_config(), &mut playlist, Some(&keepers), &HashSet::new()), 1);
        let titles: Vec<_> = playlist[0].channels.iter().map(|c| c.header.title.to_string()).collect();
        assert_eq!(titles, vec!["News HD", "News 4K"], "survivors keep playlist order");
    }

    #[test]
    fn dedup_never_drops_group_item() {
        let group_item = make_item("ZDF HD");
        let excluded = make_item("ZDF UHD");
        let skip: HashSet<UUIDType> = [group_item.header.uuid].into_iter().collect();
        let mut playlist = vec![make_group("News", vec![group_item, excluded])];
        let removed = deduplicate_playlist(&DeduplicateConfig::default(), &mut playlist, None, &skip);
        assert_eq!(removed, 0);
        assert_eq!(playlist[0].channels.len(), 2);
    }
}
