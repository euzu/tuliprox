use crate::model::AppConfig;
use crate::repository::bplustree::BPlusTreeQuery;
use crate::repository::xtream_repository::xtream_get_file_path;
use crate::utils::FileReadGuard;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use log::{error, warn};
use serde::{Deserialize, Serialize};
use shared::model::{M3uPlaylistItem, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemType, UUIDType, XtreamCluster, XtreamPlaylistItem};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub trait PlaylistSource: Send + Sync {
    fn is_memory(&self) -> bool;
    fn get_channel_count(&mut self) -> usize;
    fn get_group_count(&mut self) -> usize;
    fn is_empty(&mut self) -> bool;
    #[allow(clippy::wrong_self_convention)]
    fn into_items(&mut self) -> Box<dyn Iterator<Item=PlaylistItem> + Send + '_>;
    fn items_mut(&mut self) -> Box<dyn Iterator<Item=&mut PlaylistItem> + Send + '_>;
    fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item=std::borrow::Cow<'a, PlaylistItem>> + Send + 'a>;
    fn update_playlist<'a>(&'a mut self, plg: &'a PlaylistGroup) -> BoxFuture<'a, ()>;
    fn get_missing_vod_info_count(&mut self) -> usize;
    fn get_missing_series_info_count(&mut self) -> usize;
    fn deduplicate(&mut self, duplicates: &mut HashSet<UUIDType>);
    fn take_groups(&mut self) -> Vec<PlaylistGroup>;
    fn clone_box(&self) -> Box<dyn PlaylistSource>;
    fn release_resources(&mut self, cluster: XtreamCluster);
    fn obtain_resources(&mut self) -> BoxFuture<'_, ()>;
    fn sort_by_provider_ordinal(&mut self);
}

#[derive(Default)]
pub struct EmptyPlaylistSource {}

impl PlaylistSource for EmptyPlaylistSource {
    fn is_memory(&self) -> bool { true }
    fn get_channel_count(&mut self) -> usize { 0 }
    fn get_group_count(&mut self) -> usize { 0 }
    fn is_empty(&mut self) -> bool { true }
    fn into_items(&mut self) -> Box<dyn Iterator<Item=PlaylistItem> + Send + '_> { Box::new(std::iter::empty()) }
    fn items_mut(&mut self) -> Box<dyn Iterator<Item=&mut PlaylistItem> + Send + '_> { Box::new(std::iter::empty()) }
    fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item=Cow<'a, PlaylistItem>> + Send + 'a> { Box::new(std::iter::empty()) }
    fn update_playlist<'a>(&'a mut self, _plg: &'a PlaylistGroup) -> BoxFuture<'a, ()> { Box::pin(async move {}) }
    fn get_missing_vod_info_count(&mut self) -> usize { 0 }
    fn get_missing_series_info_count(&mut self) -> usize { 0 }
    fn deduplicate(&mut self, _duplicates: &mut HashSet<UUIDType>) { /* noop */ }
    fn take_groups(&mut self) -> Vec<PlaylistGroup> { vec![] }
    fn clone_box(&self) -> Box<dyn PlaylistSource> { Box::new(EmptyPlaylistSource::default()) }
    fn release_resources(&mut self, _cluster: XtreamCluster) { /* noop */ }
    fn obtain_resources(&mut self) -> BoxFuture<'_, ()> { Box::pin(async move {}) }
    fn sort_by_provider_ordinal(&mut self) { /* noop */ }
}

pub struct XtreamDiskPlaylistSource {
    app_config: Arc<AppConfig>,
    storage_path: PathBuf,
    live: Option<(BPlusTreeQuery<u32, XtreamPlaylistItem>, Arc<FileReadGuard>)>,
    vod: Option<(BPlusTreeQuery<u32, XtreamPlaylistItem>, Arc<FileReadGuard>)>,
    series: Option<(BPlusTreeQuery<u32, XtreamPlaylistItem>, Arc<FileReadGuard>)>,
}

impl XtreamDiskPlaylistSource {
    pub(crate) async fn new(app_config: &Arc<AppConfig>, storage_path: &Path) -> Self {
        let mut source = XtreamDiskPlaylistSource {
            app_config: Arc::clone(app_config),
            storage_path: storage_path.to_path_buf(),
            live: None,
            vod: None,
            series: None,
        };
        source.reload().await;
        source
    }

    async fn reload(&mut self) {
        if self.live.is_none() {
            let live_path = xtream_get_file_path(&self.storage_path, XtreamCluster::Live);
            self.live = load_bplustree_query::<u32, XtreamPlaylistItem>(&self.app_config, &live_path).await
                .map(|(query, guard)| (query, Arc::new(guard)));
        }
        if self.vod.is_none() {
            let vod_path = xtream_get_file_path(&self.storage_path, XtreamCluster::Video);
            self.vod = load_bplustree_query::<u32, XtreamPlaylistItem>(&self.app_config, &vod_path).await
                .map(|(query, guard)| (query, Arc::new(guard)));
        }

        if self.series.is_none() {
            let series_path = xtream_get_file_path(&self.storage_path, XtreamCluster::Series);
            self.series = load_bplustree_query::<u32, XtreamPlaylistItem>(&self.app_config, &series_path).await
                .map(|(query, guard)| (query, Arc::new(guard)));
        }
    }
}

impl PlaylistSource for XtreamDiskPlaylistSource {
    fn is_memory(&self) -> bool { false }

    fn get_channel_count(&mut self) -> usize {
        self.live.as_mut().map_or(0usize, |(t, _)| t.len().unwrap_or(0usize))
            + self.vod.as_mut().map_or(0usize, |(t, _)| t.len().unwrap_or(0usize))
            + self.series.as_mut().map_or(0usize, |(t, _)| t.len().unwrap_or(0usize))
    }

    fn get_group_count(&mut self) -> usize {
        let mut groups = HashSet::new();
        if let Some((query, _)) = self.live.as_mut() { for (_, item) in query.iter() { groups.insert(item.group.clone()); } }
        if let Some((query, _)) = self.vod.as_mut() { for (_, item) in query.iter() { groups.insert(item.group.clone()); } }
        if let Some((query, _)) = self.series.as_mut() { for (_, item) in query.iter() { groups.insert(item.group.clone()); } }
        groups.len()
    }

    fn is_empty(&mut self) -> bool {
        self.live.as_mut().is_none_or(|(q, _)| q.is_empty().unwrap_or(true))
            && self.vod.as_mut().is_none_or(|(q, _)| q.is_empty().unwrap_or(true))
            && self.series.as_mut().is_none_or(|(q, _)| q.is_empty().unwrap_or(true))
    }

    fn into_items(&mut self) -> Box<dyn Iterator<Item=PlaylistItem> + Send + '_> {
        let live = self.live.as_mut().into_iter().flat_map(|(q, _)| q.iter()).map(|(_, item)| PlaylistItem::from(&item));
        let vod = self.vod.as_mut().into_iter().flat_map(|(q, _)| q.iter()).map(|(_, item)| PlaylistItem::from(&item));
        let series = self.series.as_mut().into_iter().flat_map(|(q, _)| q.iter()).map(|(_, item)| PlaylistItem::from(&item));
        Box::new(live.chain(vod).chain(series))
    }

    fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item=Cow<'a, PlaylistItem>> + Send + 'a> {
        let live = self.live.as_mut().into_iter().flat_map(|(q, _)| q.iter()).map(|(_, item)| Cow::Owned(PlaylistItem::from(&item)));
        let vod = self.vod.as_mut().into_iter().flat_map(|(q, _)| q.iter()).map(|(_, item)| Cow::Owned(PlaylistItem::from(&item)));
        let series = self.series.as_mut().into_iter().flat_map(|(q, _)| q.iter()).map(|(_, item)| Cow::Owned(PlaylistItem::from(&item)));
        Box::new(live.chain(vod).chain(series))
    }

    fn items_mut(&mut self) -> Box<dyn Iterator<Item=&mut PlaylistItem> + Send + '_> {
        warn!("Disk-based playlist sources are read-only. Use clone_source() and convert to memory for mutable access.");
        Box::new(std::iter::empty())
    }

    fn update_playlist<'a>(&'a mut self, _plg: &'a PlaylistGroup) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            warn!("update_playlist should not be called for Xtream Disk playlist");
            // // Drop read guards before write lock
            // self.live = None;
            // self.vod = None;
            // self.series = None;
            //
            // let xtream_path = xtream_get_file_path(&self.storage_path, plg.xtream_cluster);
            // {
            //     let _lock = self.app_config.file_locks.write_lock(&xtream_path).await;
            //     if let Ok(mut tree) = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
            //         let xtream_items: Vec<XtreamPlaylistItem> = plg.channels.iter().map(XtreamPlaylistItem::from).collect();
            //         let batch: Vec<(&u32, &XtreamPlaylistItem)> = xtream_items.iter().map(|item| (&item.virtual_id, item)).collect();
            //         let _ = tree.upsert_batch(&batch);
            //     }
            // }
            // self.reload().await;
        })
    }

    fn get_missing_vod_info_count(&mut self) -> usize {
        let mut count = 0;
        if let Some((query, _)) = self.vod.as_mut() {
            for (_, item) in query.iter() {
                if item.item_type == PlaylistItemType::Video && item.provider_id > 0 && !item.has_details() {
                    count += 1;
                }
            }
        }
        count
    }

    fn get_missing_series_info_count(&mut self) -> usize {
        let mut count = 0;
        if let Some((query, _)) = self.series.as_mut() {
            for (_, item) in query.iter() {
                if item.item_type == PlaylistItemType::SeriesInfo && item.provider_id > 0 && !item.has_details() {
                    count += 1;
                }
            }
        }
        count
    }

    fn deduplicate(&mut self, _duplicates: &mut HashSet<UUIDType>) {
        warn!("Deduplication is not supported for disk based playlist updates");
    }

    fn take_groups(&mut self) -> Vec<PlaylistGroup> {
        // Build groups on-the-fly using disk iterator (streams one leaf at a time)
        let mut groups_map: IndexMap<u32, PlaylistGroup> = IndexMap::new();
        let mut iters: Vec<(XtreamCluster, Box<dyn Iterator<Item=XtreamPlaylistItem> + Send>)> = vec![];
        if let Some((q, _)) = self.live.as_mut() {
            iters.push((XtreamCluster::Live, Box::new(q.iter().map(|(_, item)| item))));
        }
        if let Some((q, _)) = self.vod.as_mut() {
            iters.push((XtreamCluster::Video, Box::new(q.iter().map(|(_, item)| item))));
        }
        if let Some((q, _)) = self.series.as_mut() {
            iters.push((XtreamCluster::Series, Box::new(q.iter().map(|(_, item)| item))));
        }

        for (cluster, iter) in iters {
            for item in iter {
                groups_map.entry(item.category_id)
                    .or_insert_with(|| PlaylistGroup {
                        id: item.category_id,
                        title: item.group.clone(),
                        channels: vec![],
                        xtream_cluster: cluster,
                    })
                    .channels.push(PlaylistItem::from(&item));
            }
        }

        // Sort channels within each group
        for group in groups_map.values_mut() {
            group.channels.sort_by_key(|item| item.header.source_ordinal);
        }

        // Sort groups based on the source_ordinal of their first channel
        let mut groups: Vec<PlaylistGroup> = groups_map.into_values().collect();
        groups.sort_by_key(|group| {
            group.channels.first().map_or(u32::MAX, |c| c.header.source_ordinal)
        });
        groups
    }

    fn clone_box(&self) -> Box<dyn PlaylistSource> {
        let live_path = xtream_get_file_path(&self.storage_path, XtreamCluster::Live);
        let vod_path = xtream_get_file_path(&self.storage_path, XtreamCluster::Video);
        let series_path = xtream_get_file_path(&self.storage_path, XtreamCluster::Series);

        let live = self.live.as_ref().and_then(|(_, guard)| {
            BPlusTreeQuery::try_new(&live_path).ok().map(|q| (q, Arc::clone(guard)))
        });
        let vod = self.vod.as_ref().and_then(|(_, guard)| {
            BPlusTreeQuery::try_new(&vod_path).ok().map(|q| (q, Arc::clone(guard)))
        });
        let series = self.series.as_ref().and_then(|(_, guard)| {
            BPlusTreeQuery::try_new(&series_path).ok().map(|q| (q, Arc::clone(guard)))
        });

        Box::new(Self {
            app_config: Arc::clone(&self.app_config),
            storage_path: self.storage_path.clone(),
            live,
            vod,
            series,
        })
    }

    fn release_resources(&mut self, cluster: XtreamCluster) {
        match cluster {
            XtreamCluster::Live => self.live = None,
            XtreamCluster::Video => self.vod = None,
            XtreamCluster::Series => self.series = None,
        }
    }

    fn obtain_resources(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.reload().await;
        })
    }
    fn sort_by_provider_ordinal(&mut self) {
        warn!("Sorting by provider ordinal is not supported for disk based playlists");
    }
}

macro_rules! impl_single_file_disk_source {
    ($name:ident, $key_type:tt, $entry_type:tt) => {
      paste::paste! {
          pub struct [<$name DiskPlaylistSource>] {
            app_config: Arc<AppConfig>,
            file_path: PathBuf,
            playlist: Option<BPlusTreeQuery<$key_type, $entry_type >>,
            guard: Option<Arc<FileReadGuard>>,
          }

          impl [<$name DiskPlaylistSource>] {
            pub(crate) async fn new(app_config: &Arc<AppConfig>, file_path: &Path) -> Self {
                let mut source = Self {
                    app_config: Arc::clone(app_config),
                    file_path: file_path.to_path_buf(),
                    playlist: None,
                    guard: None,
                };
                source.reload().await;
                source
            }

            async fn reload(&mut self) {
                self.guard = None;
                self.playlist = load_bplustree_query::<$key_type, $entry_type>(&self.app_config, &self.file_path).await
                    .map(|(query, guard)| {
                        self.guard = Some(Arc::new(guard));
                        query
                    });
            }
        }

        impl PlaylistSource for [<$name DiskPlaylistSource>] {

            fn get_channel_count(&mut self) -> usize { self.playlist.as_mut().map_or(0usize, |t: &mut BPlusTreeQuery<$key_type, $entry_type>| t.len().unwrap_or(0usize)) }

            fn is_memory(&self) -> bool { false }

            fn get_group_count(&mut self) -> usize {
                let mut groups = HashSet::new();
                if let Some(query) = self.playlist.as_mut() { for (_, item) in query.iter() { groups.insert(item.group.clone()); } }
                groups.len()
            }

            fn is_empty(&mut self) -> bool { self.playlist.as_mut().map_or(true, |t| t.is_empty().unwrap_or(true)) }

            fn into_items(&mut self) -> Box<dyn Iterator<Item=PlaylistItem> + Send + '_> {
                if let Some(q) = self.playlist.as_mut() {
                    Box::new(q.iter().map(|(_, item)| PlaylistItem::from(&item)))
                } else {
                    Box::new(std::iter::empty())
                }
            }


            fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item=Cow<'a, PlaylistItem>> + Send + 'a> {
                if let Some(pl) = self.playlist.as_mut() {
                    let iter = pl.iter().map(|(_, item)| Cow::Owned(PlaylistItem::from(&item)));
                    Box::new(iter)
                } else {
                    Box::new(std::iter::empty())
                }
            }

            fn items_mut(&mut self) -> Box<dyn Iterator<Item=&mut PlaylistItem> + Send + '_> {
                warn!("Disk-based playlist sources are read-only. Use clone_source() and convert to memory for mutable access.");
                Box::new(std::iter::empty())
            }

            fn update_playlist<'a>(&'a mut self, _plg: &'a PlaylistGroup) -> BoxFuture<'a, ()> {
                Box::pin(async move {
                    warn!("update_playlist should not be called for M3U Disk playlist");
                })
            }

            fn get_missing_vod_info_count(&mut self) -> usize { 0 }
            fn get_missing_series_info_count(&mut self) -> usize { 0 }
            fn deduplicate(&mut self, _duplicates: &mut HashSet<UUIDType>) {
                warn!("Deduplication is not supported for disk based playlist updates");
            }
            fn take_groups(&mut self) -> Vec<PlaylistGroup> {
                // Build groups on-the-fly using disk iterator (streams one leaf at a time)
                if let Some(q) = self.playlist.as_mut() {
                    let mut groups_map: IndexMap<Arc<str>, PlaylistGroup> = IndexMap::new();
                    for (_, item) in q.iter() {
                        groups_map.entry(item.group.clone())
                            .or_insert_with(|| PlaylistGroup {
                                id: 0,
                                title: item.group.clone(),
                                channels: vec![],
                                xtream_cluster: XtreamCluster::try_from(item.item_type).unwrap_or(XtreamCluster::Live),
                            })
                            .channels.push(PlaylistItem::from(&item));
                    }
                    // Sort channels within each group
                    for group in groups_map.values_mut() {
                        group.channels.sort_by_key(|item| item.header.source_ordinal);
                    }

                    // Sort groups based on the source_ordinal of their first channel
                    let mut groups: Vec<PlaylistGroup> = groups_map.into_values().collect();
                    groups.sort_by_key(|group| {
                        group.channels.first().map_or(u32::MAX, |c| c.header.source_ordinal)
                    });
                    groups
                } else {
                    vec![]
                }
            }
            fn clone_box(&self) -> Box<dyn PlaylistSource> {
                let playlist = if self.playlist.is_some() && self.guard.is_some() {
                    BPlusTreeQuery::try_new(&self.file_path).ok()
                } else { None };

                Box::new(Self {
                    app_config: Arc::clone(&self.app_config),
                    file_path: self.file_path.clone(),
                    playlist,
                    guard: self.guard.clone(),
                })
            }

            fn release_resources(&mut self, _cluster: XtreamCluster) {
                self.guard = None;
                self.playlist = None;
            }

            fn obtain_resources(&mut self) -> BoxFuture<'_, ()> {
                Box::pin(async move {
                    self.reload().await;
                })
            }

            fn sort_by_provider_ordinal(&mut self) {
                warn!("Sorting by provider ordinal is not supported for disk based playlists");
            }
         }
     }
   };
}

impl_single_file_disk_source!(M3u, String, M3uPlaylistItem);

impl_single_file_disk_source!(LocalLibrary, UUIDType, XtreamPlaylistItem);

pub struct MemoryPlaylistSource {
    playlist: Arc<Vec<PlaylistGroup>>,
}

impl MemoryPlaylistSource {
    pub(crate) fn new(groups: Vec<PlaylistGroup>) -> Self {
        Self { playlist: Arc::new(groups) }
    }

    pub fn boxed(self) -> Box<dyn PlaylistSource> {
        Box::new(self)
    }
}

impl Default for MemoryPlaylistSource {
    fn default() -> Self {
        Self { playlist: Arc::new(vec![]) }
    }
}

impl PlaylistSource for MemoryPlaylistSource {
    fn is_memory(&self) -> bool { true }
    fn get_channel_count(&mut self) -> usize { self.playlist.iter().map(|group| group.channels.len()).sum() }
    fn get_group_count(&mut self) -> usize { self.playlist.len() }
    fn is_empty(&mut self) -> bool { self.playlist.is_empty() }
    fn into_items(&mut self) -> Box<dyn Iterator<Item=PlaylistItem> + Send + '_> {
        let playlist = Arc::make_mut(&mut self.playlist);
        Box::new(playlist.iter_mut().flat_map(|group| group.channels.drain(..)))
    }
    fn items_mut(&mut self) -> Box<dyn Iterator<Item=&mut PlaylistItem> + Send + '_> {
        let playlist = Arc::make_mut(&mut self.playlist);
        Box::new(playlist.iter_mut().flat_map(|group| group.channels.iter_mut()))
    }

    fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item=Cow<'a, PlaylistItem>> + Send + 'a> {
        Box::new(
            self.playlist.iter()
                .flat_map(|group| group.channels.iter().map(Cow::Borrowed))
        )
    }

    fn update_playlist<'a>(&'a mut self, plg: &'a PlaylistGroup) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let playlist = Arc::make_mut(&mut self.playlist);
            for grp in playlist.iter_mut() {
                if grp.id == plg.id {
                    grp.channels.extend(plg.channels.iter().cloned());
                    return;
                }
            }
            playlist.push(plg.clone());
        })
    }
    fn get_missing_vod_info_count(&mut self) -> usize {
        self.playlist.iter()
            .flat_map(|plg| &plg.channels)
            .filter(|pli| pli.header.xtream_cluster == XtreamCluster::Video
                && pli.header.item_type == PlaylistItemType::Video
                && !pli.has_details()).count()
    }
    fn get_missing_series_info_count(&mut self) -> usize {
        self.playlist.iter()
            .flat_map(|plg| &plg.channels)
            .filter(|&pli| pli.header.xtream_cluster == XtreamCluster::Series
                && pli.header.item_type == PlaylistItemType::SeriesInfo
                && pli.get_provider_id().is_some_and(|id| id > 0)
                && !pli.has_details()).count()
    }
    fn deduplicate(&mut self, duplicates: &mut HashSet<UUIDType>) {
        let playlist = Arc::make_mut(&mut self.playlist);
        for group in playlist {
            group.channels.retain(|item| duplicates.insert(item.get_uuid()));
        }
    }
    fn take_groups(&mut self) -> Vec<PlaylistGroup> {
        std::mem::take(Arc::make_mut(&mut self.playlist))
    }
    fn clone_box(&self) -> Box<dyn PlaylistSource> {
        Box::new(MemoryPlaylistSource { playlist: Arc::clone(&self.playlist) })
    }
    fn release_resources(&mut self, _cluster: XtreamCluster) { /* noop */ }
    fn obtain_resources(&mut self) -> BoxFuture<'_, ()> { Box::pin(async move {}) }

    fn sort_by_provider_ordinal(&mut self) {
        let playlist = Arc::make_mut(&mut self.playlist);
        for group in &mut *playlist {
            group.channels.sort_by_key(|item| item.header.source_ordinal);
        }
        playlist.sort_by_key(|group|
            group.channels.first().map_or(u32::MAX, |c| c.header.source_ordinal)
        );
    }
}

async fn load_bplustree_query<K, P>(app_config: &Arc<AppConfig>, file_path: &Path) -> Option<(BPlusTreeQuery<K, P>, FileReadGuard)>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    P: Serialize + for<'de> Deserialize<'de> + Clone + Send + 'static,
{
    if file_path.exists() {
        let guard = app_config.file_locks.read_lock(file_path).await;
        match BPlusTreeQuery::<K, P>::try_new(file_path) {
            Ok(query) => Some((query, guard)),
            Err(err) => {
                error!("Error loading disk playlist {}: {err}", file_path.display());
                None
            }
        }
    } else { None }
}