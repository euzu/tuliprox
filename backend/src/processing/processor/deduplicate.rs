use shared::model::{DeduplicateConfig, DeduplicateKeep, DeduplicateMatchBy, PlaylistGroup, PlaylistItem, XtreamCluster};
use std::collections::HashMap;

/// Quality tiers recognized inside channel captions, best first.
fn token_quality(token: &str) -> Option<u8> {
    const TIERS: &[(&[&str], u8)] = &[
        (&["4K", "UHD", "2160P"], 5),
        (&["QHD", "1440P"], 4),
        (&["FHD", "1080P"], 3),
        (&["HD", "720P"], 2),
        (&["SD", "480P", "576P"], 1),
    ];
    TIERS
        .iter()
        .find_map(|(tokens, rank)| tokens.iter().any(|t| token.eq_ignore_ascii_case(t)).then_some(*rank))
}

fn value_tokens(value: &str) -> impl Iterator<Item = &str> {
    value.split(|c: char| !c.is_alphanumeric()).filter(|token| !token.is_empty())
}

/// Best quality tier found in `value`; 0 when no known token is present.
pub(in crate::processing::processor) fn quality_rank(value: &str) -> u8 {
    value_tokens(value).filter_map(token_quality).max().unwrap_or(0)
}

/// Lowercased token join with quality tokens removed, so "News HD" and
/// "NEWS [FHD]" produce the same key.
fn normalized_dedup_key(value: &str) -> String {
    let mut key = String::with_capacity(value.len());
    for token in value_tokens(value) {
        if token_quality(token).is_some() {
            continue;
        }
        if !key.is_empty() {
            key.push(' ');
        }
        for ch in token.chars() {
            key.extend(ch.to_lowercase());
        }
    }
    key
}

fn match_value(config: &DeduplicateConfig, item: &PlaylistItem) -> String {
    let header = &item.header;
    let value = match config.match_by {
        DeduplicateMatchBy::Caption => {
            if header.title.is_empty() {
                header.name.as_ref()
            } else {
                header.title.as_ref()
            }
        }
        DeduplicateMatchBy::Name => header.name.as_ref(),
        DeduplicateMatchBy::Title => header.title.as_ref(),
    };
    normalized_dedup_key(value)
}

fn raw_match_value<'a>(config: &DeduplicateConfig, item: &'a PlaylistItem) -> &'a str {
    let header = &item.header;
    match config.match_by {
        DeduplicateMatchBy::Caption => {
            if header.title.is_empty() {
                header.name.as_ref()
            } else {
                header.title.as_ref()
            }
        }
        DeduplicateMatchBy::Name => header.name.as_ref(),
        DeduplicateMatchBy::Title => header.title.as_ref(),
    }
}

/// Collapse duplicate channels across the whole target playlist (per cluster).
/// Returns the number of removed channels. Empty match keys are never
/// deduplicated; ties keep the first occurrence in playlist order.
pub(in crate::processing::processor) fn deduplicate_playlist(
    config: &DeduplicateConfig,
    playlist: &mut Vec<PlaylistGroup>,
) -> usize {
    // winner per key: (quality rank, group index, channel index)
    let mut winners: HashMap<(XtreamCluster, String), (u8, usize, usize)> = HashMap::new();
    for (group_idx, group) in playlist.iter().enumerate() {
        for (channel_idx, channel) in group.channels.iter().enumerate() {
            let key_value = match_value(config, channel);
            if key_value.is_empty() {
                continue;
            }
            let rank = quality_rank(raw_match_value(config, channel));
            match winners.entry((group.xtream_cluster, key_value)) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert((rank, group_idx, channel_idx));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if config.keep == DeduplicateKeep::BestQuality && rank > entry.get().0 {
                        entry.insert((rank, group_idx, channel_idx));
                    }
                }
            }
        }
    }

    let mut removed = 0;
    for (group_idx, group) in playlist.iter_mut().enumerate() {
        let cluster = group.xtream_cluster;
        let before = group.channels.len();
        let mut channel_idx = 0usize;
        group.channels.retain(|channel| {
            let idx = channel_idx;
            channel_idx += 1;
            let key_value = match_value(config, channel);
            if key_value.is_empty() {
                return true;
            }
            winners
                .get(&(cluster, key_value))
                .is_none_or(|(_, winner_group, winner_channel)| *winner_group == group_idx && *winner_channel == idx)
        });
        removed += before - group.channels.len();
    }
    playlist.retain(|group| !group.channels.is_empty());
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PlaylistItemHeader;
    use shared::utils::Internable;

    fn make_item(title: &str) -> PlaylistItem {
        PlaylistItem { header: PlaylistItemHeader { title: title.intern(), ..Default::default() } }
    }

    fn make_group(title: &str, channels: Vec<PlaylistItem>) -> PlaylistGroup {
        PlaylistGroup {
            id: 1,
            title: title.intern(),
            channels,
            xtream_cluster: XtreamCluster::Live,
        }
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
        let config =
            DeduplicateConfig { match_by: DeduplicateMatchBy::Caption, keep: DeduplicateKeep::BestQuality };
        let removed = deduplicate_playlist(&config, &mut playlist);
        assert_eq!(removed, 2);
        let titles: Vec<_> = playlist[0].channels.iter().map(|c| c.header.title.to_string()).collect();
        assert_eq!(titles, vec!["News [FHD]", "Sports HD"]);
    }

    #[test]
    fn dedup_keep_first_preserves_playlist_order_winner() {
        let mut playlist =
            vec![make_group("G", vec![make_item("News HD"), make_item("News [FHD]")])];
        let config = DeduplicateConfig { match_by: DeduplicateMatchBy::Caption, keep: DeduplicateKeep::First };
        let removed = deduplicate_playlist(&config, &mut playlist);
        assert_eq!(removed, 1);
        assert_eq!(playlist[0].channels[0].header.title.as_ref(), "News HD");
    }

    #[test]
    fn dedup_ignores_empty_keys_and_drops_empty_groups() {
        let mut playlist = vec![
            make_group("A", vec![make_item("News HD")]),
            make_group("B", vec![make_item("News FHD")]),
        ];
        let config =
            DeduplicateConfig { match_by: DeduplicateMatchBy::Caption, keep: DeduplicateKeep::BestQuality };
        let removed = deduplicate_playlist(&config, &mut playlist);
        assert_eq!(removed, 1);
        assert_eq!(playlist.len(), 1);
        assert_eq!(playlist[0].title.as_ref(), "B");
    }
}
