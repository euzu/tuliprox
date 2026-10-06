//! Group index next to a target EPG db: playlist group -> live channels with an EPG key.
//!
//! The EPG db is keyed by EPG id, so it cannot answer "which channels belong to group X"
//! without a full scan. This index is built in the same step as the EPG db, holds no
//! programmes, and lets the Web UI stream one group at a time.

use crate::{storage_const, BPlusTree};
use serde::{Deserialize, Serialize};
use shared::{
    concat_string,
    error::TuliproxError,
    model::{PlaylistGroup, XtreamCluster},
};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::utils::{canonicalize_output_epg_id, EpgIdOutputCase};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpgGroupEntry {
    pub name: Arc<str>,
    pub channel_count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpgGroupChannel {
    pub virtual_id: u32,
    pub name: Arc<str>,
    pub logo: Arc<str>,
    pub epg_key: Arc<str>,
}

/// `(group name, position in group)`
pub type EpgGroupChannelKey = (Arc<str>, u32);

pub type EpgGroupTree = BPlusTree<u32, EpgGroupEntry>;
pub type EpgGroupChannelTree = BPlusTree<EpgGroupChannelKey, EpgGroupChannel>;

/// Sibling of the EPG db: `epg_groups.db`, keyed by group order.
pub fn epg_groups_path(epg_path: &Path) -> PathBuf {
    epg_path.with_file_name(concat_string!("epg_groups.", storage_const::FILE_SUFFIX_DB))
}

/// Sibling of the EPG db: `epg_group_channels.db`, keyed by `(group, position)`.
pub fn epg_group_channels_path(epg_path: &Path) -> PathBuf {
    epg_path.with_file_name(concat_string!("epg_group_channels.", storage_const::FILE_SUFFIX_DB))
}

/// Builds both index trees from an output playlist. Only live channels whose canonical EPG key
/// is in `epg_keys` (written to the EPG db, i.e. with programmes) are indexed. Groups keep the
/// order of their first appearance; groups without an indexed channel are omitted.
pub fn build_epg_group_index<S: std::hash::BuildHasher>(
    playlist: &[PlaylistGroup],
    epg_keys: &HashSet<Arc<str>, S>,
    output_case: EpgIdOutputCase,
) -> (EpgGroupTree, EpgGroupChannelTree) {
    // group title -> (order of first appearance, indexed channel count)
    let mut groups_by_title: HashMap<Arc<str>, (u32, u32)> = HashMap::new();
    let mut next_order = 0u32;
    let mut channels = EpgGroupChannelTree::new();
    for group in playlist.iter().filter(|group| group.xtream_cluster == XtreamCluster::Live) {
        let (_, count) = groups_by_title.entry(Arc::clone(&group.title)).or_insert_with(|| {
            next_order += 1;
            (next_order - 1, 0)
        });
        for item in &group.channels {
            let header = &item.header;
            let Some(epg_id) = header.epg_channel_id.as_ref().filter(|id| !id.is_empty()) else {
                continue;
            };
            let epg_key = canonicalize_output_epg_id(epg_id, output_case);
            if !epg_keys.contains(&epg_key) {
                continue;
            }
            let logo = if header.logo.is_empty() { &header.logo_small } else { &header.logo };
            channels.insert(
                (Arc::clone(&group.title), *count),
                EpgGroupChannel {
                    virtual_id: header.virtual_id.get(),
                    name: Arc::clone(&header.name),
                    logo: Arc::clone(logo),
                    epg_key,
                },
            );
            *count += 1;
        }
    }
    let mut groups = EpgGroupTree::new();
    for (name, (order, channel_count)) in groups_by_title {
        if channel_count > 0 {
            groups.insert(order, EpgGroupEntry { name, channel_count });
        }
    }
    (groups, channels)
}

/// Stores a built group index next to the EPG db at `epg_path`.
pub fn epg_group_index_store(
    (mut groups, mut channels): (EpgGroupTree, EpgGroupChannelTree),
    epg_path: &Path,
) -> Result<(), TuliproxError> {
    let to_error = |path: &Path, err: io::Error| {
        TuliproxError::RepositoryEpg(format!("Failed to write epg group index {}: {err}", path.display()))
    };
    let groups_path = epg_groups_path(epg_path);
    let channels_path = epg_group_channels_path(epg_path);
    // Channels first: a reader that sees the new groups file must find its channels.
    channels.store(&channels_path).map_err(|err| to_error(&channels_path, err))?;
    groups.store(&groups_path).map_err(|err| to_error(&groups_path, err))?;
    Ok(())
}

/// Removes the group index next to the EPG db at `epg_path`, so it cannot describe a rewritten
/// EPG db it was not built for. Missing files are fine.
pub fn epg_group_index_remove(epg_path: &Path) -> Result<(), TuliproxError> {
    // Groups first: readers treat a missing groups file as "no index".
    for path in [epg_groups_path(epg_path), epg_group_channels_path(epg_path)] {
        match std::fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return Err(TuliproxError::RepositoryEpg(format!(
                    "Failed to remove epg group index {}: {err}",
                    path.display()
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Builds and stores the group index next to the EPG db at `epg_path`.
pub fn epg_group_index_write<S: std::hash::BuildHasher>(
    playlist: &[PlaylistGroup],
    epg_keys: &HashSet<Arc<str>, S>,
    output_case: EpgIdOutputCase,
    epg_path: &Path,
) -> Result<(), TuliproxError> {
    epg_group_index_store(build_epg_group_index(playlist, epg_keys, output_case), epg_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BPlusTreeQuery;
    use shared::{
        model::{PlaylistItem, PlaylistItemHeader, VirtualId},
        utils::Internable,
    };
    use tempfile::TempDir;

    fn channel(virtual_id: u32, name: &str, epg: Option<&str>) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(virtual_id),
                name: name.intern(),
                epg_channel_id: epg.map(Internable::intern),
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn group(id: u32, title: &str, cluster: XtreamCluster, channels: Vec<PlaylistItem>) -> PlaylistGroup {
        PlaylistGroup { id, title: title.intern(), channels, xtream_cluster: cluster }
    }

    fn keys(ids: &[&str]) -> HashSet<Arc<str>> { ids.iter().map(|id| id.intern()).collect() }

    fn groups_of(tree: &EpgGroupTree) -> Vec<(String, u32)> {
        tree.iter().map(|(_, group)| (group.name.to_string(), group.channel_count)).collect()
    }

    #[test]
    fn groups_keep_first_appearance_order_even_if_first_channel_has_no_epg() {
        let playlist = vec![
            group(1, "A", XtreamCluster::Live, vec![channel(1, "a0", None)]),
            group(2, "B", XtreamCluster::Live, vec![channel(2, "b1", Some("b1"))]),
            group(3, "A", XtreamCluster::Live, vec![channel(3, "a1", Some("a1"))]),
        ];
        let (groups, _) = build_epg_group_index(&playlist, &keys(&["a1", "b1"]), EpgIdOutputCase::Preserve);
        assert_eq!(groups_of(&groups), vec![("A".into(), 1), ("B".into(), 1)]);
    }

    #[test]
    fn only_live_channels_with_written_epg_are_indexed() {
        let playlist = vec![
            group(
                1,
                "News",
                XtreamCluster::Live,
                vec![channel(1, "n1", Some("n1")), channel(2, "n2", Some("")), channel(3, "n3", Some("no-programmes"))],
            ),
            group(2, "Movies", XtreamCluster::Video, vec![channel(4, "m", Some("n1"))]),
            group(3, "Kids", XtreamCluster::Live, vec![channel(5, "k", None)]),
        ];
        let (groups, channels) = build_epg_group_index(&playlist, &keys(&["n1"]), EpgIdOutputCase::Preserve);
        assert_eq!(groups_of(&groups), vec![("News".into(), 1)]);
        let rows: Vec<_> =
            channels.iter().map(|((group, pos), channel)| (group.to_string(), *pos, channel.virtual_id)).collect();
        assert_eq!(rows, vec![("News".into(), 0, 1)]);
    }

    #[test]
    fn shared_epg_id_is_indexed_in_every_group() {
        let playlist = vec![
            group(1, "HD", XtreamCluster::Live, vec![channel(1, "ARD HD", Some("ard"))]),
            group(2, "SD", XtreamCluster::Live, vec![channel(2, "ARD", Some("ard"))]),
        ];
        let (_, channels) = build_epg_group_index(&playlist, &keys(&["ard"]), EpgIdOutputCase::Preserve);
        let mut virtual_ids: Vec<_> = channels.iter().map(|(_, channel)| channel.virtual_id).collect();
        virtual_ids.sort_unstable();
        assert_eq!(virtual_ids, vec![1, 2]);
    }

    #[test]
    fn epg_keys_use_output_case() {
        let playlist = vec![group(1, "G", XtreamCluster::Live, vec![channel(1, "c", Some("Das.Erste"))])];
        let (_, channels) = build_epg_group_index(&playlist, &keys(&["das.erste"]), EpgIdOutputCase::LowercaseAscii);
        assert_eq!(channels.iter().next().map(|(_, c)| c.epg_key.to_string()), Some("das.erste".into()));
    }

    #[test]
    fn positions_continue_when_group_title_repeats() {
        let playlist = vec![
            group(1, "G", XtreamCluster::Live, vec![channel(1, "a", Some("a"))]),
            group(2, "G", XtreamCluster::Live, vec![channel(2, "b", Some("b"))]),
        ];
        let (groups, channels) = build_epg_group_index(&playlist, &keys(&["a", "b"]), EpgIdOutputCase::Preserve);
        assert_eq!(groups_of(&groups), vec![("G".into(), 2)]);
        let positions: Vec<_> = channels.iter().map(|((_, pos), channel)| (*pos, channel.virtual_id)).collect();
        assert_eq!(positions, vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn logo_falls_back_to_small_logo() {
        let mut item = channel(1, "c", Some("c"));
        item.header.logo_small = "small.png".intern();
        let playlist = vec![group(1, "G", XtreamCluster::Live, vec![item])];
        let (_, channels) = build_epg_group_index(&playlist, &keys(&["c"]), EpgIdOutputCase::Preserve);
        assert_eq!(channels.iter().next().map(|(_, c)| c.logo.to_string()), Some("small.png".into()));
    }

    #[test]
    fn written_index_is_readable_including_group_range() {
        let tmp = TempDir::new().expect("temp dir created");
        let epg_path = tmp.path().join("epg.db");
        let playlist = vec![
            group(1, "News", XtreamCluster::Live, vec![channel(1, "n1", Some("n1")), channel(2, "n2", Some("n2"))]),
            group(2, "Newsroom", XtreamCluster::Live, vec![channel(3, "r", Some("r"))]),
        ];
        epg_group_index_write(&playlist, &keys(&["n1", "n2", "r"]), EpgIdOutputCase::Preserve, &epg_path)
            .expect("index written");

        let mut groups =
            BPlusTreeQuery::<u32, EpgGroupEntry>::try_new(&epg_groups_path(&epg_path)).expect("groups index opened");
        let names: Vec<_> = groups
            .iter()
            .map(|entry| entry.map(|(_, group)| (group.name.to_string(), group.channel_count)))
            .collect::<io::Result<_>>()
            .expect("groups readable");
        assert_eq!(names, vec![("News".into(), 2), ("Newsroom".into(), 1)]);

        let mut channels =
            BPlusTreeQuery::<EpgGroupChannelKey, EpgGroupChannel>::try_new(&epg_group_channels_path(&epg_path))
                .expect("channel index opened");
        let start: EpgGroupChannelKey = ("News".intern(), 0);
        let end: EpgGroupChannelKey = ("News".intern(), u32::MAX);
        let rows: Vec<_> = channels
            .range_iter(std::ops::Bound::Included(&start), std::ops::Bound::Included(&end))
            .map(|entry| entry.map(|(_, channel)| channel.virtual_id))
            .collect::<io::Result<_>>()
            .expect("channels readable");
        assert_eq!(rows, vec![1, 2]);
    }

    #[test]
    fn empty_index_is_written_and_readable() {
        let tmp = TempDir::new().expect("temp dir created");
        let epg_path = tmp.path().join("epg.db");
        epg_group_index_write(&[], &HashSet::new(), EpgIdOutputCase::Preserve, &epg_path).expect("index written");
        let mut groups =
            BPlusTreeQuery::<u32, EpgGroupEntry>::try_new(&epg_groups_path(&epg_path)).expect("groups index opened");
        assert_eq!(groups.iter().count(), 0);
    }
}
