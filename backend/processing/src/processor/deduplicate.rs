use super::match_key::{dedup_match_by, extract_normalized_match_key, raw_match_value};
use indexmap::IndexMap;
use shared::{
    model::{
        ChannelMatchBy, DeduplicateConfig, DeduplicateKeep, DeduplicateMatchBy, PlaylistGroup, PlaylistItem,
        PluginDuplicateGroup, PluginKeepers, StreamProperties, UUIDType, XtreamCluster,
    },
    utils::quality_rank,
};
use std::collections::HashSet;

fn match_value(config: &DeduplicateConfig, item: &PlaylistItem) -> String {
    if let Some(match_by) = dedup_match_by(config.match_by) {
        return extract_normalized_match_key(&item.header, match_by, config.match_as_ascii, None);
    }
    if matches!(item.header.additional_properties.as_ref(), Some(StreamProperties::Episode(_))) {
        return String::new();
    }
    let tmdb_id = match item.header.additional_properties.as_ref() {
        Some(StreamProperties::Video(video)) => video.tmdb,
        Some(StreamProperties::Series(series)) => series.tmdb,
        _ => None,
    };
    if let Some(id) = tmdb_id.filter(|&id| id > 0) {
        return format!("tmdb:{id}");
    }
    if config.match_by == DeduplicateMatchBy::TmdbThenCaption {
        let mut norm = extract_normalized_match_key(&item.header, ChannelMatchBy::Caption, config.match_as_ascii, None);
        if !norm.is_empty() {
            norm.insert_str(0, "caption:");
            return norm;
        }
    }
    String::new()
}

fn item_raw_quality_value<'a>(config: &DeduplicateConfig, item: &'a PlaylistItem) -> &'a str {
    match dedup_match_by(config.match_by) {
        Some(match_by) => raw_match_value(&item.header, match_by),
        None => raw_match_value(&item.header, ChannelMatchBy::Caption),
    }
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

type DuplicateGroupKey = (XtreamCluster, Option<usize>, String);

/// The sets of two or more duplicates in `playlist`, ordered by their first
/// member. Pure: nothing is removed. Empty match keys never form a set.
pub(in crate::processor) fn group_duplicates(
    config: &DeduplicateConfig,
    playlist: &[PlaylistGroup],
    skip: &HashSet<UUIDType>,
) -> Vec<DuplicateSet> {
    let mut sets: IndexMap<DuplicateGroupKey, Vec<(usize, usize)>> = IndexMap::new();
    let group_scoped = matches!(config.match_by, DeduplicateMatchBy::Tmdb | DeduplicateMatchBy::TmdbThenCaption);
    for (group_idx, group) in playlist.iter().enumerate() {
        for (channel_idx, channel) in group.channels.iter().enumerate() {
            if should_skip(channel, skip) {
                continue;
            }
            let key_value = match_value(config, channel);
            if key_value.is_empty() {
                continue;
            }
            let scope_idx = if group_scoped { Some(group_idx) } else { None };
            sets.entry((group.xtream_cluster, scope_idx, key_value)).or_default().push((group_idx, channel_idx));
        }
    }
    sets.into_iter()
        .filter(|(_, members)| members.len() > 1)
        .map(|((_, _, key), members)| DuplicateSet { key, members })
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
        let rank = quality_rank(item_raw_quality_value(config, &playlist[group].channels[channel]));
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
        for (member, &(group, channel)) in set.members.iter().enumerate() {
            if !kept.contains(&member) {
                losers.insert((group, channel));
                if log::log_enabled!(log::Level::Debug) {
                    let removed_title = &playlist[group].channels[channel].header.title;
                    let kept_title = kept.first().map_or("", |&first_kept| {
                        let (kg, kc) = set.members[first_kept];
                        playlist[kg].channels[kc].header.title.as_ref()
                    });
                    log::debug!(
                        "deduplicate removed duplicate channel '{removed_title}', kept '{kept_title}' (key: '{}')",
                        set.key
                    );
                }
            }
        }
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
        model::{
            DeduplicateMatchBy, EpisodeStreamProperties, PlaylistItemHeader, SeriesStreamProperties, StreamProperties,
            VideoStreamProperties,
        },
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

    fn make_vod_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                xtream_cluster: XtreamCluster::Video,
                additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                    tmdb,
                    ..VideoStreamProperties::default()
                }))),
                ..Default::default()
            },
        }
    }

    fn make_vod_group(title: &str, channels: Vec<PlaylistItem>) -> PlaylistGroup {
        PlaylistGroup { id: 1, title: title.intern(), channels, xtream_cluster: XtreamCluster::Video }
    }

    #[test]
    fn tmdb_collapses_two_vod_items_with_same_tmdb_id_and_different_titles_inside_one_group_and_keeps_better_quality_by_caption(
    ) {
        let mut playlist = vec![make_vod_group(
            "Sci-Fi",
            vec![make_vod_item("Matrix Revolutions HD", Some(605)), make_vod_item("Matrix 3 [4K]", Some(605))],
        )];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Tmdb,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 1);
        assert_eq!(playlist[0].channels.len(), 1);
        assert_eq!(playlist[0].channels[0].header.title.as_ref(), "Matrix 3 [4K]");
    }

    #[test]
    fn tmdb_same_tmdb_id_in_two_groups_stays() {
        let mut playlist = vec![
            make_vod_group("Favorites", vec![make_vod_item("The Matrix HD", Some(603))]),
            make_vod_group("Sci-Fi", vec![make_vod_item("The Matrix [4K]", Some(603))]),
        ];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Tmdb,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 0);
        assert_eq!(playlist.len(), 2);
        assert_eq!(playlist[0].channels.len(), 1);
        assert_eq!(playlist[1].channels.len(), 1);
    }

    #[test]
    fn tmdb_items_without_tmdb_id_untouched() {
        let mut playlist = vec![make_vod_group(
            "Indie",
            vec![
                make_vod_item("Indie Film HD", None),
                make_vod_item("Indie Film [4K]", None),
                make_vod_item("Zero TMDB HD", Some(0)),
                make_vod_item("Zero TMDB [4K]", Some(0)),
            ],
        )];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Tmdb,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 0);
        assert_eq!(playlist[0].channels.len(), 4);
    }

    #[test]
    fn tmdb_then_caption_collapses_same_tmdb_preserves_remakes_and_collapses_missing_tmdb_within_group_only() {
        let mut playlist = vec![
            make_vod_group(
                "Movies",
                vec![
                    // Same TMDB id collapses
                    make_vod_item("Dune HD", Some(438_631)),
                    make_vod_item("Dune [4K]", Some(438_631)),
                    // Two remakes with the same caption but different TMDB ids stay
                    make_vod_item("Total Recall HD", Some(861)),
                    make_vod_item("Total Recall [FHD]", Some(64_635)),
                    // Items without TMDB id collapse by caption within the group only
                    make_vod_item("Local Special HD", None),
                    make_vod_item("Local Special FHD", None),
                ],
            ),
            make_vod_group(
                "Bonus",
                vec![
                    // Same caption as in group 1, but no TMDB id; remains untouched due to group scope
                    make_vod_item("Local Special SD", None),
                ],
            ),
        ];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::TmdbThenCaption,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 2);
        let group0_titles: Vec<_> = playlist[0].channels.iter().map(|c| c.header.title.to_string()).collect();
        assert_eq!(group0_titles, vec!["Dune [4K]", "Total Recall HD", "Total Recall [FHD]", "Local Special FHD"]);
        let group1_titles: Vec<_> = playlist[1].channels.iter().map(|c| c.header.title.to_string()).collect();
        assert_eq!(group1_titles, vec!["Local Special SD"]);
    }

    #[test]
    fn tmdb_live_items_untouched() {
        let mut playlist = vec![make_group("Live TV", vec![make_item("News HD"), make_item("News FHD")])];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Tmdb,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 0);
        assert_eq!(playlist[0].channels.len(), 2);
    }

    fn make_series_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                xtream_cluster: XtreamCluster::Series,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    tmdb,
                    ..SeriesStreamProperties::default()
                }))),
                ..Default::default()
            },
        }
    }

    fn make_episode_item(title: &str, tmdb: Option<u32>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                title: title.intern(),
                xtream_cluster: XtreamCluster::Series,
                additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                    episode_id: 1,
                    episode: 1,
                    season: 1,
                    added: None,
                    release_date: None,
                    series_release_date: None,
                    plot: None,
                    tmdb,
                    movie_image: "".intern(),
                    container_extension: "mp4".intern(),
                    video: None,
                    audio: None,
                }))),
                ..Default::default()
            },
        }
    }

    fn make_series_group(title: &str, channels: Vec<PlaylistItem>) -> PlaylistGroup {
        PlaylistGroup { id: 1, title: title.intern(), channels, xtream_cluster: XtreamCluster::Series }
    }

    #[test]
    fn tmdb_series_episodes_sharing_series_tmdb_id_are_not_deduplicated() {
        let mut playlist = vec![make_series_group(
            "Breaking Bad",
            vec![
                make_series_item("Breaking Bad", Some(1396)),
                make_episode_item("Breaking Bad S01E01", Some(1396)),
                make_episode_item("Breaking Bad S01E02", Some(1396)),
                make_episode_item("Breaking Bad S01E03", Some(1396)),
            ],
        )];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Tmdb,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 0);
        assert_eq!(playlist[0].channels.len(), 4);
    }

    #[test]
    fn tmdb_then_caption_series_episodes_sharing_series_tmdb_id_fall_back_to_distinct_captions() {
        let mut playlist = vec![make_series_group(
            "Breaking Bad",
            vec![
                make_series_item("Breaking Bad", Some(1396)),
                make_episode_item("Breaking Bad S01E01", Some(1396)),
                make_episode_item("Breaking Bad S01E02", Some(1396)),
                make_episode_item("Breaking Bad S01E03", Some(1396)),
            ],
        )];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::TmdbThenCaption,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 0);
        assert_eq!(playlist[0].channels.len(), 4);
    }

    #[test]
    fn tmdb_collapses_duplicate_series_info_items_in_same_group() {
        let mut playlist = vec![make_series_group(
            "Breaking Bad",
            vec![make_series_item("Breaking Bad HD", Some(1396)), make_series_item("Breaking Bad [4K]", Some(1396))],
        )];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::Tmdb,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 1);
        assert_eq!(playlist[0].channels.len(), 1);
        assert_eq!(playlist[0].channels[0].header.title.as_ref(), "Breaking Bad [4K]");
    }

    #[test]
    fn tmdb_then_caption_does_not_deduplicate_episodes_sharing_title_with_different_series_tmdb() {
        let mut playlist = vec![make_series_group(
            "Pilots",
            vec![make_episode_item("Pilot", Some(1396)), make_episode_item("Pilot", Some(1399))],
        )];
        let config = DeduplicateConfig {
            match_by: DeduplicateMatchBy::TmdbThenCaption,
            keep: DeduplicateKeep::BestQuality,
            match_as_ascii: false,
        };
        let removed = deduplicate_playlist(&config, &mut playlist, None, &HashSet::new());
        assert_eq!(removed, 0);
        assert_eq!(playlist[0].channels.len(), 2);
    }
}
