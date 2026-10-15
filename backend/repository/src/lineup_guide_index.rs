//! `GuideNumber` index of the `HDHomeRun` lineup of a target.
//!
//! `lineup.json` publishes one guide number per channel — the EPG channel id, else the channel
//! name — and the tuner endpoints receive that string back as `/auto/{channel}`. Answering it by
//! walking the playlist iterator reads the complete target playlist per tune (measured: 0.4 to
//! 0.8 s for 50 000 channels), while the keyed lookup by virtual id this index feeds takes about
//! a millisecond.
//!
//! The index is a sidecar of one target playlist view, written by the same persistence step that
//! writes the playlist it describes, and only for targets that publish an `HDHomeRun` output. A
//! missing index is therefore not an error: readers fall back to the scan, which stays the
//! reference implementation.
//!
//! Every feed of a guide number is kept, not only the one the lineup publishes first. The tuner
//! serves the first feed that this user's output publishes, and channel failover will pick among
//! the same list by quality of service instead.

use crate::{m3u_get_file_path_for_db, storage_const, xtream_get_file_path, xtream_get_storage_path};
use serde::{Deserialize, Serialize};
use shared::{
    concat_string,
    error::TuliproxError,
    model::{PlaylistGroup, PlaylistItemType, TargetType, XtreamCluster},
};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_btree::{BPlusTree, BPlusTreeQuery};
use tuliprox_core::{
    model::{Config, ConfigTarget},
    utils::FileLockManager,
};

/// Target playlist view a lineup is published from.
///
/// `lineup.json` serves either the M3U target playlist or the Xtream target playlists, selected
/// by the `use_output` of the `HDHomeRun` output, so each view needs its own index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineupGuideView {
    /// The flat M3U target playlist: one key space holding every item type.
    M3u,
    /// The Xtream target playlists: live and video, each in its own db.
    Xtream,
}

/// The item a guide number resolves to.
///
/// A guide number usually names one channel, but providers publish one EPG id for several feeds of
/// the same channel (SD, HD, FHD, backup), which is also what channel failover groups. The index
/// therefore keeps every feed of a guide number, ordered the way the lineup scan meets them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineupGuideEntry {
    /// Cluster holding the virtual id, needed to fetch from the right Xtream db.
    pub cluster: XtreamCluster,
    pub virtual_id: u32,
}

/// Guide number to every [`LineupGuideEntry`] carrying it.
pub type LineupGuideTree = BPlusTree<Arc<str>, Vec<LineupGuideEntry>>;

/// The candidates one requested key has, in the order the lineup meets them.
#[derive(Debug, Clone, PartialEq)]
pub struct LineupGuideCandidate {
    /// The lookup key that matched, so the caller can verify the item still carries it.
    pub key: Arc<str>,
    pub entries: Vec<LineupGuideEntry>,
}

/// Outcome of a guide number lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum LineupGuideLookup {
    /// No usable index: the target never published an `HDHomeRun` lineup, or a persistence step is
    /// between removing the old index and publishing the new one.
    Missing,
    /// The index exists but knows none of the requested keys.
    NoEntry,
    /// Candidates of every requested key the index knows, in the order the keys were asked for, so
    /// the caller can tell a guide number a channel carries from the stripped form of another.
    Candidates(Vec<LineupGuideCandidate>),
}

/// View `lineup.json` is published from, selected by the `use_output` of the `HDHomeRun` output.
///
/// Without a `use_output` the M3U playlist wins over the Xtream playlists. `None` means the target
/// has no output a lineup can be published from.
///
/// This deliberately does not require the target to carry the `HDHomeRun` output itself: the device
/// serves the target of its user, and `lineup.json` has always published that target's playlist
/// with the default selection when the output is not on it. Whether a guide index exists is the
/// separate question [`lineup_guide_index_view`] answers.
pub fn hdhomerun_lineup_view(target: &ConfigTarget) -> Option<LineupGuideView> {
    let use_output = target.get_hdhomerun_output().and_then(|output| output.use_output);
    let has_m3u = target.has_output(TargetType::M3u);
    let has_xtream = target.has_output(TargetType::Xtream);
    match use_output {
        None | Some(TargetType::M3u) if has_m3u => Some(LineupGuideView::M3u),
        None | Some(TargetType::Xtream) if has_xtream => Some(LineupGuideView::Xtream),
        _ => None,
    }
}

/// View whose guide index a target keeps: only a target publishing an `HDHomeRun` output has one.
pub fn lineup_guide_index_view(target: &ConfigTarget) -> Option<LineupGuideView> {
    if target.has_output(TargetType::HdHomeRun) {
        hdhomerun_lineup_view(target)
    } else {
        None
    }
}

fn lineup_guide_file_name() -> String { concat_string!("lineup_guide.", storage_const::FILE_SUFFIX_DB) }

/// Index file of the M3U view, next to the M3U playlist db of the target.
pub fn m3u_lineup_guide_index_path(target_path: &Path) -> PathBuf {
    m3u_get_file_path_for_db(target_path).with_file_name(lineup_guide_file_name())
}

/// Index file of the Xtream view, next to the live playlist db of the target.
pub fn xtream_lineup_guide_index_path(config: &Config, target_name: &str) -> Option<PathBuf> {
    xtream_get_storage_path(config, target_name)
        .map(|path| xtream_get_file_path(&path, XtreamCluster::Live).with_file_name(lineup_guide_file_name()))
}

/// Rank of a cluster when the lineup of the Xtream view is published: the live db is searched
/// before the video db, so live wins a guide number two channels share.
fn cluster_rank(cluster: XtreamCluster) -> u8 {
    match cluster {
        XtreamCluster::Live => 0,
        XtreamCluster::Video => 1,
        XtreamCluster::Series => 2,
    }
}

/// Cluster rank, source ordinal and virtual id: the order the lineup meets its items in.
type LineupPosition = (u8, u32, u32);

/// Position of an item in the lineup of `view`.
///
/// The playlist dbs are read through their sorted index, which orders by `source_ordinal` (see
/// `m3u_write_playlist` and the Xtream publication). The M3U db is one key space, the Xtream view is
/// live first and then video. The virtual id breaks ties the way the db key does.
fn lineup_position(
    view: LineupGuideView,
    cluster: XtreamCluster,
    source_ordinal: u32,
    virtual_id: u32,
) -> LineupPosition {
    let cluster_rank = match view {
        LineupGuideView::M3u => 0,
        LineupGuideView::Xtream => cluster_rank(cluster),
    };
    (cluster_rank, source_ordinal, virtual_id)
}

/// One index row: guide number, lineup position and the entry.
type LineupGuideRow = (Arc<str>, LineupPosition, LineupGuideEntry);

/// Entries of clusters a persistence step keeps, read from the index that already describes them.
///
/// An Xtream update without items for one cluster keeps that cluster's db, so the new index cannot
/// learn its channels from the update. They are carried over from the previous index instead: it
/// was written together with the kept db, which has not changed since.
#[derive(Debug, Default)]
pub struct LineupGuideCarryOver {
    rows: Vec<LineupGuideRow>,
}

/// Rows of the playlist channels the index of `view` holds.
fn playlist_rows(playlist: &[PlaylistGroup], view: LineupGuideView) -> Vec<LineupGuideRow> {
    playlist
        .iter()
        .flat_map(|group| &group.channels)
        .filter_map(|item| {
            let header = &item.header;
            let cluster = header.xtream_cluster;
            let indexed = match view {
                LineupGuideView::M3u => {
                    !matches!(header.item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo)
                }
                LineupGuideView::Xtream => matches!(cluster, XtreamCluster::Live | XtreamCluster::Video),
            };
            if !indexed {
                return None;
            }
            let guide = header.epg_channel_id.as_ref().unwrap_or(&header.name);
            if guide.is_empty() {
                return None;
            }
            let virtual_id = header.virtual_id.get();
            Some((
                Arc::clone(guide),
                lineup_position(view, cluster, header.source_ordinal, virtual_id),
                LineupGuideEntry { cluster, virtual_id },
            ))
        })
        .collect()
}

/// Groups rows by guide number into the tree, each group in lineup order.
///
/// The rows are one flat list sorted by guide number and lineup position, so grouping needs no map
/// with one vector per guide number next to the tree being built.
fn tree_from_rows(mut rows: Vec<LineupGuideRow>) -> LineupGuideTree {
    rows.sort_unstable_by(|left, right| (&left.0, left.1).cmp(&(&right.0, right.1)));

    let mut tree = LineupGuideTree::new();
    let mut rows = rows.into_iter().peekable();
    while let Some((guide, _, entry)) = rows.next() {
        let mut entries = vec![entry];
        while let Some((_, _, entry)) = rows.next_if(|(next, _, _)| *next == guide) {
            // The same item listed in several groups carries different source ordinals, so its
            // rows need not be adjacent. A guide number has only a handful of feeds, which keeps
            // this check cheap; the first occurrence keeps its lineup position.
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
        tree.insert(guide, entries);
    }
    tree
}

/// Builds the index of one target playlist view.
///
/// The Xtream view indexes live and video only, matching the channels `lineup.json` publishes for
/// it. The M3U view indexes what its db holds: every item except series info, which
/// `m3u_write_playlist` leaves out.
///
/// The guide number is the one `lineup.json` publishes: the EPG channel id when it is set,
/// otherwise the name. An empty EPG channel id publishes the empty guide number, which no request
/// can produce, so such items are left out.
pub fn build_lineup_guide_index(playlist: &[PlaylistGroup], view: LineupGuideView) -> LineupGuideTree {
    tree_from_rows(playlist_rows(playlist, view))
}

/// Reads the entries of `clusters` from the index at `path`, to carry them into the next index.
///
/// Only the rows of the kept clusters are held; the rest of the index is streamed past. Each keeps
/// its position within its guide number, which is lineup order: the clusters rank live before
/// video, and the previous index already lists the feeds of one cluster in lineup order. `None`
/// when the index is missing or unreadable, so the caller cannot describe the kept clusters.
pub async fn lineup_guide_index_carry_over(
    file_locks: &FileLockManager,
    path: &Path,
    clusters: &[XtreamCluster],
) -> Option<LineupGuideCarryOver> {
    let file_lock = file_locks.read_lock(path).await;
    let path = path.to_path_buf();
    let clusters = clusters.to_vec();
    let task = tokio::task::spawn_blocking(move || -> io::Result<LineupGuideCarryOver> {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<Arc<str>, Vec<LineupGuideEntry>>::try_new(&path)?;
        let mut rows = Vec::new();
        for entry in query.iter() {
            let (guide, entries) = entry?;
            for (ordinal, entry) in (0u32..).zip(entries) {
                if clusters.contains(&entry.cluster) {
                    let position = (cluster_rank(entry.cluster), ordinal, entry.virtual_id);
                    rows.push((Arc::clone(&guide), position, entry));
                }
            }
        }
        Ok(LineupGuideCarryOver { rows })
    });
    match task.await {
        Ok(Ok(carry_over)) => Some(carry_over),
        Ok(Err(err)) => {
            log::debug!("No lineup guide index to carry over: {err}");
            None
        }
        Err(err) => {
            log::warn!("Lineup guide index carry over task failed: {err}");
            None
        }
    }
}

/// Builds the index of `view` and stores it at `path`, together with the entries `carry_over`
/// brings from the previous index.
///
/// Only collecting the rows from the borrowed playlist runs on the calling task. Sorting, building
/// the tree, creating the directory and the store run on a blocking thread under the write lock of
/// the index file, so a reader never opens a file the store is still copying (the store falls back
/// to a copy when it cannot rename).
pub async fn lineup_guide_index_write(
    file_locks: &FileLockManager,
    playlist: &[PlaylistGroup],
    view: LineupGuideView,
    path: &Path,
    carry_over: Option<LineupGuideCarryOver>,
) -> Result<(), TuliproxError> {
    let mut rows = playlist_rows(playlist, view);
    if let Some(carry_over) = carry_over {
        rows.extend(carry_over.rows);
    }
    let file_lock = file_locks.write_lock(path).await;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        let mut tree = tree_from_rows(rows);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                TuliproxError::RepositoryPlaylist(format!(
                    "Failed to create lineup guide index directory {}: {err}",
                    parent.display()
                ))
            })?;
        }
        tree.store(&path).map(|_| ()).map_err(|err| {
            TuliproxError::RepositoryPlaylist(format!("Failed to write lineup guide index {}: {err}", path.display()))
        })
    })
    .await
    .map_err(|err| TuliproxError::RepositoryPlaylist(format!("Lineup guide index write task failed: {err}")))?
}

/// Removes the indexes at `paths`, so they cannot describe a playlist they were not built for.
///
/// A missing file is fine: it is what a reader expects before the first persistence step, and
/// what a failed write leaves behind. Every target persistence step asks for this, so the common
/// case of no index at all costs one blocking hop for the existence checks and takes no lock.
pub async fn lineup_guide_index_remove(file_locks: &FileLockManager, paths: &[PathBuf]) -> Result<(), TuliproxError> {
    let candidates = paths.to_vec();
    let existing = tokio::task::spawn_blocking(move || {
        // An unanswerable existence check is treated as "exists", so the removal is still tried.
        candidates.into_iter().filter(|path| path.try_exists().unwrap_or(true)).collect::<Vec<_>>()
    })
    .await
    .map_err(|err| TuliproxError::RepositoryPlaylist(format!("Lineup guide index removal task failed: {err}")))?;

    for path in existing {
        let _guard = file_locks.write_lock(&path).await;
        match tokio::fs::remove_file(&path).await {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return Err(TuliproxError::RepositoryPlaylist(format!(
                    "Failed to remove lineup guide index {}: {err}",
                    path.display()
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Every candidate the index holds for `keys`, grouped per key in the order the keys were given.
///
/// Runs the file access on a blocking thread, so all keys are answered from one open index. An
/// index that cannot be opened is reported as [`LineupGuideLookup::Missing`] so the caller uses its
/// scan instead of failing the request.
pub async fn lineup_guide_candidates(
    file_locks: &FileLockManager,
    path: PathBuf,
    keys: Vec<Arc<str>>,
) -> LineupGuideLookup {
    let file_lock = file_locks.read_lock(&path).await;
    let task = tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        let Ok(mut query) = BPlusTreeQuery::<Arc<str>, Vec<LineupGuideEntry>>::try_new(&path) else {
            return LineupGuideLookup::Missing;
        };
        let mut candidates = Vec::with_capacity(keys.len());
        for key in keys {
            match query.query(&key) {
                Ok(Some(entries)) => candidates.push(LineupGuideCandidate { key, entries }),
                Ok(None) => {}
                Err(err) => {
                    log::warn!("Failed to query lineup guide index {}: {err}", path.display());
                    return LineupGuideLookup::Missing;
                }
            }
        }
        if candidates.is_empty() {
            LineupGuideLookup::NoEntry
        } else {
            LineupGuideLookup::Candidates(candidates)
        }
    });

    match task.await {
        Ok(lookup) => lookup,
        // A lookup that could not run is "no index", so the caller falls back to its scan.
        Err(err) => {
            log::warn!("Lineup guide index lookup task failed: {err}");
            LineupGuideLookup::Missing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{
        model::{PlaylistItem, PlaylistItemHeader, VirtualId},
        utils::Internable,
    };
    use tempfile::TempDir;
    use tuliprox_core::utils::FileLockManager;

    fn channel(virtual_id: u32, name: &str, epg: Option<&str>, cluster: XtreamCluster) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(virtual_id),
                name: name.intern(),
                epg_channel_id: epg.map(Internable::intern),
                xtream_cluster: cluster,
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn group(id: u32, cluster: XtreamCluster, channels: Vec<PlaylistItem>) -> PlaylistGroup {
        PlaylistGroup { id, title: "Group".intern(), channels, xtream_cluster: cluster }
    }

    /// Index content as `(guide number, candidates)`, ordered for comparison.
    fn entries(tree: &LineupGuideTree) -> Vec<(String, Vec<(String, u32)>)> {
        let mut entries: Vec<_> = tree
            .iter()
            .map(|(guide, candidates)| {
                let candidates = candidates.iter().map(|entry| (entry.cluster.to_string(), entry.virtual_id)).collect();
                (guide.to_string(), candidates)
            })
            .collect();
        entries.sort();
        entries
    }

    #[test]
    fn m3u_view_keeps_every_feed_of_a_guide_number_in_lineup_order() {
        let playlist = vec![
            group(1, XtreamCluster::Live, vec![channel(9, "live feed", Some("shared.de"), XtreamCluster::Live)]),
            group(
                2,
                XtreamCluster::Video,
                vec![
                    channel(4, "vod feed", Some("shared.de"), XtreamCluster::Video),
                    channel(5, "without epg", None, XtreamCluster::Video),
                ],
            ),
        ];

        let tree = build_lineup_guide_index(&playlist, LineupGuideView::M3u);

        // The M3U playlist is one key space; with equal source ordinals the virtual id decides,
        // and a channel without an EPG id is published under its name.
        assert_eq!(
            entries(&tree),
            vec![
                ("shared.de".to_string(), vec![("video".to_string(), 4), ("live".to_string(), 9)]),
                ("without epg".to_string(), vec![("video".to_string(), 5)]),
            ]
        );
        let candidates = tree.query(&"shared.de".intern()).expect("guide number is indexed");
        assert_eq!(candidates.first().map(|entry| entry.virtual_id), Some(4));
    }

    #[test]
    fn feeds_follow_the_source_order_of_the_playlist_db_not_the_virtual_id() {
        let mut hd = channel(3, "HD", Some("shared.de"), XtreamCluster::Live);
        hd.header.source_ordinal = 1;
        let mut sd = channel(1, "SD", Some("shared.de"), XtreamCluster::Live);
        sd.header.source_ordinal = 2;
        let mut vod = channel(2, "VOD", Some("shared.de"), XtreamCluster::Video);
        vod.header.source_ordinal = 0;
        let playlist = vec![group(1, XtreamCluster::Live, vec![sd, hd]), group(2, XtreamCluster::Video, vec![vod])];

        // The dbs are read through their sorted index by source ordinal: the M3U db as one key
        // space, the Xtream view live before video.
        let ids = |view| {
            build_lineup_guide_index(&playlist, view)
                .query(&"shared.de".intern())
                .map(|entries| entries.iter().map(|entry| entry.virtual_id).collect::<Vec<_>>())
        };
        assert_eq!(ids(LineupGuideView::M3u), Some(vec![2, 3, 1]));
        assert_eq!(ids(LineupGuideView::Xtream), Some(vec![3, 1, 2]));
    }

    #[test]
    fn m3u_view_leaves_series_info_out() {
        let mut series = channel(1, "series", Some("series.de"), XtreamCluster::Series);
        series.header.item_type = PlaylistItemType::SeriesInfo;
        let playlist = vec![group(1, XtreamCluster::Series, vec![series])];

        // `m3u_write_playlist` does not store series info, so the index must not point at it.
        assert!(entries(&build_lineup_guide_index(&playlist, LineupGuideView::M3u)).is_empty());
    }

    #[test]
    fn xtream_view_lists_live_before_video_and_leaves_series_out() {
        let playlist = vec![
            group(1, XtreamCluster::Live, vec![channel(9, "live feed", Some("shared.de"), XtreamCluster::Live)]),
            group(
                2,
                XtreamCluster::Video,
                vec![
                    channel(4, "vod feed", Some("shared.de"), XtreamCluster::Video),
                    channel(5, "smaller id", Some("only.vod"), XtreamCluster::Video),
                    channel(6, "smaller id again", Some("only.vod"), XtreamCluster::Video),
                ],
            ),
            group(3, XtreamCluster::Series, vec![channel(1, "series", Some("series.de"), XtreamCluster::Series)]),
        ];

        let tree = build_lineup_guide_index(&playlist, LineupGuideView::Xtream);

        // The lineup searches the live db before the video db, and the whole series db is not part
        // of the lineup at all. Within one cluster the smaller id comes first.
        assert_eq!(
            entries(&tree),
            vec![
                ("only.vod".to_string(), vec![("video".to_string(), 5), ("video".to_string(), 6)]),
                ("shared.de".to_string(), vec![("live".to_string(), 9), ("video".to_string(), 4)]),
            ]
        );
    }

    #[test]
    fn empty_epg_channel_id_is_not_indexed_under_the_name() {
        let playlist = vec![group(
            1,
            XtreamCluster::Live,
            vec![channel(1, "named", Some(""), XtreamCluster::Live), channel(2, "", None, XtreamCluster::Live)],
        )];

        let tree = build_lineup_guide_index(&playlist, LineupGuideView::M3u);

        // `lineup.json` publishes the empty EPG id as the guide number of the first channel, which
        // no request can ask for, so neither the empty id nor the name may answer for it.
        assert!(entries(&tree).is_empty());
    }

    #[tokio::test]
    async fn index_round_trips_and_remove_tolerates_missing_files() {
        let temp = TempDir::new().expect("temp dir created");
        let path = temp.path().join("lineup_guide.db");
        let locks = FileLockManager::default();
        let playlist =
            vec![group(1, XtreamCluster::Live, vec![channel(7, "VOX", Some("vox.de"), XtreamCluster::Live)])];

        assert_eq!(
            lineup_guide_candidates(&locks, path.clone(), vec!["vox.de".intern()]).await,
            LineupGuideLookup::Missing
        );
        lineup_guide_index_write(&locks, &playlist, LineupGuideView::M3u, &path, None).await.expect("index written");
        assert_eq!(
            lineup_guide_candidates(&locks, path.clone(), vec!["missing.de".intern(), "vox.de".intern()]).await,
            LineupGuideLookup::Candidates(vec![LineupGuideCandidate {
                key: "vox.de".intern(),
                entries: vec![LineupGuideEntry { cluster: XtreamCluster::Live, virtual_id: 7 }],
            }])
        );
        assert_eq!(
            lineup_guide_candidates(&locks, path.clone(), vec!["missing.de".intern()]).await,
            LineupGuideLookup::NoEntry
        );

        lineup_guide_index_remove(&locks, std::slice::from_ref(&path)).await.expect("index removed");
        lineup_guide_index_remove(&locks, std::slice::from_ref(&path)).await.expect("removing a missing index is fine");
        assert_eq!(lineup_guide_candidates(&locks, path, vec!["vox.de".intern()]).await, LineupGuideLookup::Missing);
    }

    #[tokio::test]
    async fn carry_over_keeps_the_feed_order_of_the_kept_cluster_after_the_rewritten_one() {
        let temp = TempDir::new().expect("temp dir created");
        let path = temp.path().join("lineup_guide.db");
        let locks = FileLockManager::default();
        let mut first = channel(5, "VOD HD", Some("shared.de"), XtreamCluster::Video);
        first.header.source_ordinal = 1;
        let mut second = channel(4, "VOD SD", Some("shared.de"), XtreamCluster::Video);
        second.header.source_ordinal = 2;
        let previous = vec![
            group(1, XtreamCluster::Live, vec![channel(9, "old live", Some("shared.de"), XtreamCluster::Live)]),
            group(
                2,
                XtreamCluster::Video,
                vec![first, second, channel(6, "film", Some("film.de"), XtreamCluster::Video)],
            ),
        ];
        lineup_guide_index_write(&locks, &previous, LineupGuideView::Xtream, &path, None).await.expect("index written");

        // The update rewrites live only: the old live entry goes, the video entries stay in order.
        let carry_over = lineup_guide_index_carry_over(&locks, &path, &[XtreamCluster::Video])
            .await
            .expect("previous index readable");
        let update =
            vec![group(1, XtreamCluster::Live, vec![channel(7, "new live", Some("shared.de"), XtreamCluster::Live)])];
        lineup_guide_index_write(&locks, &update, LineupGuideView::Xtream, &path, Some(carry_over))
            .await
            .expect("index rewritten");

        let tree = BPlusTreeQuery::<Arc<str>, Vec<LineupGuideEntry>>::try_new(&path)
            .expect("index opens")
            .query(&"shared.de".intern());
        let ids =
            tree.expect("query runs").map(|entries| entries.iter().map(|entry| entry.virtual_id).collect::<Vec<_>>());
        assert_eq!(ids, Some(vec![7, 5, 4]));
        assert!(lineup_guide_index_carry_over(&locks, &temp.path().join("missing.db"), &[XtreamCluster::Video])
            .await
            .is_none());
    }

    #[test]
    fn an_item_listed_in_several_groups_is_indexed_once() {
        let mut first = channel(5, "VOX", Some("vox.de"), XtreamCluster::Live);
        first.header.source_ordinal = 1;
        let mut other = channel(7, "VOX HD", Some("vox.de"), XtreamCluster::Live);
        other.header.source_ordinal = 2;
        let mut again = channel(5, "VOX", Some("vox.de"), XtreamCluster::Live);
        again.header.source_ordinal = 3;
        let playlist =
            vec![group(1, XtreamCluster::Live, vec![first, other]), group(2, XtreamCluster::Live, vec![again])];

        // The second listing of virtual id 5 is not adjacent to the first after sorting, the other
        // feed lies between them.
        let ids = build_lineup_guide_index(&playlist, LineupGuideView::M3u)
            .query(&"vox.de".intern())
            .map(|entries| entries.iter().map(|entry| entry.virtual_id).collect::<Vec<_>>());
        assert_eq!(ids, Some(vec![5, 7]));
    }
}
