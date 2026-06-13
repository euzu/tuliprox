use crate::model::AppConfig;
use crate::repository::BPlusTreeQuery;
use crate::repository::xtream_get_file_path;
use crate::utils::{normalized_source_ordinal, FileReadGuard};
use futures::future::BoxFuture;
use indexmap::IndexMap;
use log::{error, warn};
use serde::{Deserialize, Serialize};
use shared::error::TuliproxError;
use shared::model::{M3uPlaylistItem, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemType, XtreamCluster, XtreamPlaylistItem};
use shared::model::UUIDType;
use shared::utils::Internable;
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

trait PlaylistSourceOps: Send + Sync {
    fn is_memory(&self) -> bool;
    fn get_channel_count(&mut self) -> usize;
    fn get_group_count(&mut self) -> usize;
    fn get_channel_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize;
    fn get_group_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize;
    fn is_empty(&mut self) -> bool;
    #[allow(clippy::wrong_self_convention)]
    fn into_items(&mut self) -> Box<dyn Iterator<Item = PlaylistItem> + Send + '_>;
    fn items_mut(&mut self) -> Box<dyn Iterator<Item = &mut PlaylistItem> + Send + '_>;
    fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item = std::borrow::Cow<'a, PlaylistItem>> + Send + 'a>;
    fn update_playlist<'a>(&'a mut self, plg: &'a PlaylistGroup) -> BoxFuture<'a, ()>;
    fn get_missing_vod_info_count(&mut self) -> usize;
    fn get_missing_series_info_count(&mut self) -> usize;
    fn get_missing_vod_info_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize;
    fn get_missing_series_info_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize;
    fn deduplicate(&mut self, duplicates: &mut HashSet<UUIDType>);
    fn take_groups(&mut self) -> Vec<PlaylistGroup>;
    fn clone_source(&self) -> Result<PlaylistSource, TuliproxError>;
    fn release_resources(&mut self, cluster: XtreamCluster);
    fn obtain_resources(&mut self) -> BoxFuture<'_, ()>;
    fn sort_by_provider_ordinal(&mut self);
}

pub struct PlaylistSource {
    kind: PlaylistSourceKind,
    skip_set: Option<Arc<HashSet<XtreamCluster>>>,
}

enum PlaylistSourceKind {
    Empty(EmptyPlaylistSource),
    XtreamDisk(Box<XtreamDiskPlaylistSource>),
    M3uDisk(Box<M3uDiskPlaylistSource>),
    LocalLibraryDisk(Box<LocalLibraryDiskPlaylistSource>),
    MediaServerDisk(Box<MediaServerDiskPlaylistSource>),
    Memory(MemoryPlaylistSource),
}

type XtreamQueryHandle = (BPlusTreeQuery<u32, XtreamPlaylistItem>, Arc<FileReadGuard>);

impl Default for PlaylistSource {
    fn default() -> Self { Self::new(PlaylistSourceKind::Empty(EmptyPlaylistSource::default())) }
}

impl PlaylistSource {
    fn new(kind: PlaylistSourceKind) -> Self { Self { kind, skip_set: None } }

    pub fn xtream_disk(source: XtreamDiskPlaylistSource) -> Self { Self::new(PlaylistSourceKind::XtreamDisk(Box::new(source))) }

    pub fn m3u_disk(source: M3uDiskPlaylistSource) -> Self { Self::new(PlaylistSourceKind::M3uDisk(Box::new(source))) }

    pub fn local_library_disk(source: LocalLibraryDiskPlaylistSource) -> Self {
        Self::new(PlaylistSourceKind::LocalLibraryDisk(Box::new(source)))
    }

    pub fn media_server_disk(source: MediaServerDiskPlaylistSource) -> Self {
        Self::new(PlaylistSourceKind::MediaServerDisk(Box::new(source)))
    }

    pub fn memory(source: MemoryPlaylistSource) -> Self { Self::new(PlaylistSourceKind::Memory(source)) }

    pub fn filtered(mut inner: Self, skip_set: HashSet<XtreamCluster>) -> Self {
        if skip_set.is_empty() {
            return inner;
        }

        if let Some(existing) = &inner.skip_set {
            let mut merged = HashSet::with_capacity(existing.len() + skip_set.len());
            merged.extend(existing.iter().copied());
            merged.extend(skip_set);
            inner.skip_set = Some(Arc::new(merged));
        } else {
            inner.skip_set = Some(Arc::new(skip_set));
        }

        inner
    }

    pub fn is_memory(&self) -> bool {
        match &self.kind {
            PlaylistSourceKind::Empty(source) => source.is_memory(),
            PlaylistSourceKind::XtreamDisk(source) => source.is_memory(),
            PlaylistSourceKind::M3uDisk(source) => source.is_memory(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.is_memory(),
            PlaylistSourceKind::MediaServerDisk(source) => source.is_memory(),
            PlaylistSourceKind::Memory(source) => source.is_memory(),
        }
    }

    pub fn get_channel_count(&mut self) -> usize {
        if let Some(skip_set) = self.skip_set.as_ref() {
            return match &mut self.kind {
                PlaylistSourceKind::Empty(source) => source.get_channel_count_excluding_clusters(skip_set),
                PlaylistSourceKind::XtreamDisk(source) => source.get_channel_count_excluding_clusters(skip_set),
                PlaylistSourceKind::M3uDisk(source) => source.get_channel_count_excluding_clusters(skip_set),
                PlaylistSourceKind::LocalLibraryDisk(source) => source.get_channel_count_excluding_clusters(skip_set),
                PlaylistSourceKind::MediaServerDisk(source) => source.get_channel_count_excluding_clusters(skip_set),
                PlaylistSourceKind::Memory(source) => source.get_channel_count_excluding_clusters(skip_set),
            };
        }

        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.get_channel_count(),
            PlaylistSourceKind::XtreamDisk(source) => source.get_channel_count(),
            PlaylistSourceKind::M3uDisk(source) => source.get_channel_count(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.get_channel_count(),
            PlaylistSourceKind::MediaServerDisk(source) => source.get_channel_count(),
            PlaylistSourceKind::Memory(source) => source.get_channel_count(),
        }
    }

    pub fn get_group_count(&mut self) -> usize {
        if let Some(skip_set) = self.skip_set.as_ref() {
            return match &mut self.kind {
                PlaylistSourceKind::Empty(source) => source.get_group_count_excluding_clusters(skip_set),
                PlaylistSourceKind::XtreamDisk(source) => source.get_group_count_excluding_clusters(skip_set),
                PlaylistSourceKind::M3uDisk(source) => source.get_group_count_excluding_clusters(skip_set),
                PlaylistSourceKind::LocalLibraryDisk(source) => source.get_group_count_excluding_clusters(skip_set),
                PlaylistSourceKind::MediaServerDisk(source) => source.get_group_count_excluding_clusters(skip_set),
                PlaylistSourceKind::Memory(source) => source.get_group_count_excluding_clusters(skip_set),
            };
        }

        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.get_group_count(),
            PlaylistSourceKind::XtreamDisk(source) => source.get_group_count(),
            PlaylistSourceKind::M3uDisk(source) => source.get_group_count(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.get_group_count(),
            PlaylistSourceKind::MediaServerDisk(source) => source.get_group_count(),
            PlaylistSourceKind::Memory(source) => source.get_group_count(),
        }
    }

    pub fn is_empty(&mut self) -> bool {
        if self.skip_set.is_some() {
            return self.get_channel_count() == 0;
        }

        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.is_empty(),
            PlaylistSourceKind::XtreamDisk(source) => source.is_empty(),
            PlaylistSourceKind::M3uDisk(source) => source.is_empty(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.is_empty(),
            PlaylistSourceKind::MediaServerDisk(source) => source.is_empty(),
            PlaylistSourceKind::Memory(source) => source.is_empty(),
        }
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn into_items(&mut self) -> Box<dyn Iterator<Item = PlaylistItem> + Send + '_> {
        let iter = match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.into_items(),
            PlaylistSourceKind::XtreamDisk(source) => source.into_items(),
            PlaylistSourceKind::M3uDisk(source) => source.into_items(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.into_items(),
            PlaylistSourceKind::MediaServerDisk(source) => source.into_items(),
            PlaylistSourceKind::Memory(source) => source.into_items(),
        };

        if let Some(skip_set) = self.skip_set.clone() {
            Box::new(iter.filter(move |item| !skip_set.contains(&item.header.xtream_cluster)))
        } else {
            iter
        }
    }

    pub fn items_mut(&mut self) -> Box<dyn Iterator<Item = &mut PlaylistItem> + Send + '_> {
        let iter = match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.items_mut(),
            PlaylistSourceKind::XtreamDisk(source) => source.items_mut(),
            PlaylistSourceKind::M3uDisk(source) => source.items_mut(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.items_mut(),
            PlaylistSourceKind::MediaServerDisk(source) => source.items_mut(),
            PlaylistSourceKind::Memory(source) => source.items_mut(),
        };

        if let Some(skip_set) = self.skip_set.clone() {
            Box::new(iter.filter_map(move |item| {
                if skip_set.contains(&item.header.xtream_cluster) {
                    None
                } else {
                    Some(item)
                }
            }))
        } else {
            iter
        }
    }

    pub fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item = Cow<'a, PlaylistItem>> + Send + 'a> {
        let iter = match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.items(),
            PlaylistSourceKind::XtreamDisk(source) => source.items(),
            PlaylistSourceKind::M3uDisk(source) => source.items(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.items(),
            PlaylistSourceKind::MediaServerDisk(source) => source.items(),
            PlaylistSourceKind::Memory(source) => source.items(),
        };

        if let Some(skip_set) = self.skip_set.clone() {
            Box::new(iter.filter(move |item| !skip_set.contains(&item.as_ref().header.xtream_cluster)))
        } else {
            iter
        }
    }

    pub fn update_playlist<'a>(&'a mut self, plg: &'a PlaylistGroup) -> BoxFuture<'a, ()> {
        if self
            .skip_set
            .as_ref()
            .is_some_and(|skip_set| skip_set.contains(&plg.xtream_cluster))
        {
            return Box::pin(async move {});
        }

        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.update_playlist(plg),
            PlaylistSourceKind::XtreamDisk(source) => source.update_playlist(plg),
            PlaylistSourceKind::M3uDisk(source) => source.update_playlist(plg),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.update_playlist(plg),
            PlaylistSourceKind::MediaServerDisk(source) => source.update_playlist(plg),
            PlaylistSourceKind::Memory(source) => source.update_playlist(plg),
        }
    }

    /// Batched, indexed equivalent of repeatedly calling [`Self::update_playlist`]
    /// for in-memory playlists. Groups whose cluster is in the active skip set
    /// are dropped, matching `update_playlist`. Only the in-memory source merges
    /// groups, mirroring `FetchedPlaylist::update_playlist`.
    pub fn extend_playlist(&mut self, groups: Vec<PlaylistGroup>) {
        let filtered: Vec<PlaylistGroup> = if let Some(skip_set) = self.skip_set.as_ref() {
            groups.into_iter().filter(|plg| !skip_set.contains(&plg.xtream_cluster)).collect()
        } else {
            groups
        };
        if let PlaylistSourceKind::Memory(source) = &mut self.kind {
            source.merge_groups(filtered);
        }
    }

    /// Batched, indexed equivalent of repeatedly calling [`Self::update_playlist`]
    /// for in-memory playlists. Groups whose cluster is in the active skip set
    /// are dropped, matching `update_playlist`. Only the in-memory source merges
    /// groups, mirroring `FetchedPlaylist::update_playlist`.
    pub fn extend_playlist(&mut self, groups: Vec<PlaylistGroup>) {
        let filtered: Vec<PlaylistGroup> = if let Some(skip_set) = self.skip_set.as_ref() {
            groups.into_iter().filter(|plg| !skip_set.contains(&plg.xtream_cluster)).collect()
        } else {
            groups
        };
        if let PlaylistSourceKind::Memory(source) = &mut self.kind {
            source.merge_groups(filtered);
        }
    }

    pub fn get_missing_vod_info_count(&mut self) -> usize {
        if let Some(skip_set) = self.skip_set.as_ref() {
            return match &mut self.kind {
                PlaylistSourceKind::Empty(source) => source.get_missing_vod_info_count_excluding_clusters(skip_set),
                PlaylistSourceKind::XtreamDisk(source) => source.get_missing_vod_info_count_excluding_clusters(skip_set),
                PlaylistSourceKind::M3uDisk(source) => source.get_missing_vod_info_count_excluding_clusters(skip_set),
                PlaylistSourceKind::LocalLibraryDisk(source) => {
                    source.get_missing_vod_info_count_excluding_clusters(skip_set)
                }
                PlaylistSourceKind::MediaServerDisk(source) => source.get_missing_vod_info_count_excluding_clusters(skip_set),
                PlaylistSourceKind::Memory(source) => source.get_missing_vod_info_count_excluding_clusters(skip_set),
            };
        }

        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.get_missing_vod_info_count(),
            PlaylistSourceKind::XtreamDisk(source) => source.get_missing_vod_info_count(),
            PlaylistSourceKind::M3uDisk(source) => source.get_missing_vod_info_count(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.get_missing_vod_info_count(),
            PlaylistSourceKind::MediaServerDisk(source) => source.get_missing_vod_info_count(),
            PlaylistSourceKind::Memory(source) => source.get_missing_vod_info_count(),
        }
    }

    pub fn get_missing_series_info_count(&mut self) -> usize {
        if let Some(skip_set) = self.skip_set.as_ref() {
            return match &mut self.kind {
                PlaylistSourceKind::Empty(source) => source.get_missing_series_info_count_excluding_clusters(skip_set),
                PlaylistSourceKind::XtreamDisk(source) => {
                    source.get_missing_series_info_count_excluding_clusters(skip_set)
                }
                PlaylistSourceKind::M3uDisk(source) => source.get_missing_series_info_count_excluding_clusters(skip_set),
                PlaylistSourceKind::LocalLibraryDisk(source) => {
                    source.get_missing_series_info_count_excluding_clusters(skip_set)
                }
                PlaylistSourceKind::MediaServerDisk(source) => {
                    source.get_missing_series_info_count_excluding_clusters(skip_set)
                }
                PlaylistSourceKind::Memory(source) => source.get_missing_series_info_count_excluding_clusters(skip_set),
            };
        }

        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.get_missing_series_info_count(),
            PlaylistSourceKind::XtreamDisk(source) => source.get_missing_series_info_count(),
            PlaylistSourceKind::M3uDisk(source) => source.get_missing_series_info_count(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.get_missing_series_info_count(),
            PlaylistSourceKind::MediaServerDisk(source) => source.get_missing_series_info_count(),
            PlaylistSourceKind::Memory(source) => source.get_missing_series_info_count(),
        }
    }

    pub fn deduplicate(&mut self, duplicates: &mut HashSet<UUIDType>) {
        if self.skip_set.is_some() && self.is_memory() {
            let mut memory = MemoryPlaylistSource::new(self.take_groups()).into_source();
            memory.deduplicate(duplicates);
            *self = memory;
            return;
        }

        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.deduplicate(duplicates),
            PlaylistSourceKind::XtreamDisk(source) => source.deduplicate(duplicates),
            PlaylistSourceKind::M3uDisk(source) => source.deduplicate(duplicates),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.deduplicate(duplicates),
            PlaylistSourceKind::MediaServerDisk(source) => source.deduplicate(duplicates),
            PlaylistSourceKind::Memory(source) => source.deduplicate(duplicates),
        }
    }

    pub fn take_groups(&mut self) -> Vec<PlaylistGroup> {
        let groups = match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.take_groups(),
            PlaylistSourceKind::XtreamDisk(source) => source.take_groups(),
            PlaylistSourceKind::M3uDisk(source) => source.take_groups(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.take_groups(),
            PlaylistSourceKind::MediaServerDisk(source) => source.take_groups(),
            PlaylistSourceKind::Memory(source) => source.take_groups(),
        };

        if let Some(skip_set) = self.skip_set.clone() {
            groups.into_iter().filter_map(|group| filter_group(&skip_set, group)).collect()
        } else {
            groups
        }
    }

    pub fn clone_source(&self) -> Result<Self, TuliproxError> {
        let mut cloned = match &self.kind {
            PlaylistSourceKind::Empty(source) => source.clone_source(),
            PlaylistSourceKind::XtreamDisk(source) => source.clone_source(),
            PlaylistSourceKind::M3uDisk(source) => source.clone_source(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.clone_source(),
            PlaylistSourceKind::MediaServerDisk(source) => source.clone_source(),
            PlaylistSourceKind::Memory(source) => source.clone_source(),
        }?;
        cloned.skip_set.clone_from(&self.skip_set);
        Ok(cloned)
    }

    pub fn release_resources(&mut self, cluster: XtreamCluster) {
        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.release_resources(cluster),
            PlaylistSourceKind::XtreamDisk(source) => source.release_resources(cluster),
            PlaylistSourceKind::M3uDisk(source) => source.release_resources(cluster),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.release_resources(cluster),
            PlaylistSourceKind::MediaServerDisk(source) => source.release_resources(cluster),
            PlaylistSourceKind::Memory(source) => source.release_resources(cluster),
        }
    }

    pub fn obtain_resources(&mut self) -> BoxFuture<'_, ()> {
        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.obtain_resources(),
            PlaylistSourceKind::XtreamDisk(source) => source.obtain_resources(),
            PlaylistSourceKind::M3uDisk(source) => source.obtain_resources(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.obtain_resources(),
            PlaylistSourceKind::MediaServerDisk(source) => source.obtain_resources(),
            PlaylistSourceKind::Memory(source) => source.obtain_resources(),
        }
    }

    pub fn sort_by_provider_ordinal(&mut self) {
        match &mut self.kind {
            PlaylistSourceKind::Empty(source) => source.sort_by_provider_ordinal(),
            PlaylistSourceKind::XtreamDisk(source) => source.sort_by_provider_ordinal(),
            PlaylistSourceKind::M3uDisk(source) => source.sort_by_provider_ordinal(),
            PlaylistSourceKind::LocalLibraryDisk(source) => source.sort_by_provider_ordinal(),
            PlaylistSourceKind::MediaServerDisk(source) => source.sort_by_provider_ordinal(),
            PlaylistSourceKind::Memory(source) => source.sort_by_provider_ordinal(),
        }
    }
}

fn filter_group(skip_set: &HashSet<XtreamCluster>, mut group: PlaylistGroup) -> Option<PlaylistGroup> {
    if skip_set.contains(&group.xtream_cluster) {
        return None;
    }
    group
        .channels
        .retain(|item| !skip_set.contains(&item.header.xtream_cluster));
    if group.channels.is_empty() {
        None
    } else {
        Some(group)
    }
}

fn cluster_from_item_type(item_type: PlaylistItemType) -> XtreamCluster {
    XtreamCluster::try_from(item_type).unwrap_or(XtreamCluster::Live)
}

fn clone_xtream_query(
    label: &str,
    source: Option<&XtreamQueryHandle>,
) -> Result<Option<XtreamQueryHandle>, TuliproxError> {
    source
        .map(|(query, guard)| {
            query
                .try_clone()
                .map(|cloned_query| (cloned_query, Arc::clone(guard)))
                .map_err(|err| TuliproxError::RepositoryPlaylist(format!("Failed to clone {label} disk playlist query: {err}")))
        })
        .transpose()
}

#[derive(Default)]
pub struct EmptyPlaylistSource {}

impl PlaylistSourceOps for EmptyPlaylistSource {
    fn is_memory(&self) -> bool { true }
    fn get_channel_count(&mut self) -> usize { 0 }
    fn get_group_count(&mut self) -> usize { 0 }
    fn get_channel_count_excluding_clusters(&mut self, _skip_set: &HashSet<XtreamCluster>) -> usize { 0 }
    fn get_group_count_excluding_clusters(&mut self, _skip_set: &HashSet<XtreamCluster>) -> usize { 0 }
    fn is_empty(&mut self) -> bool { true }
    fn into_items(&mut self) -> Box<dyn Iterator<Item=PlaylistItem> + Send + '_> { Box::new(std::iter::empty()) }
    fn items_mut(&mut self) -> Box<dyn Iterator<Item=&mut PlaylistItem> + Send + '_> { Box::new(std::iter::empty()) }
    fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item=Cow<'a, PlaylistItem>> + Send + 'a> { Box::new(std::iter::empty()) }
    fn update_playlist<'a>(&'a mut self, _plg: &'a PlaylistGroup) -> BoxFuture<'a, ()> { Box::pin(async move {}) }
    fn get_missing_vod_info_count(&mut self) -> usize { 0 }
    fn get_missing_series_info_count(&mut self) -> usize { 0 }
    fn get_missing_vod_info_count_excluding_clusters(&mut self, _skip_set: &HashSet<XtreamCluster>) -> usize { 0 }
    fn get_missing_series_info_count_excluding_clusters(&mut self, _skip_set: &HashSet<XtreamCluster>) -> usize { 0 }
    fn deduplicate(&mut self, _duplicates: &mut HashSet<UUIDType>) { /* noop */ }
    fn take_groups(&mut self) -> Vec<PlaylistGroup> { vec![] }
    fn clone_source(&self) -> Result<PlaylistSource, TuliproxError> {
        Ok(PlaylistSource::new(PlaylistSourceKind::Empty(EmptyPlaylistSource::default())))
    }
    fn release_resources(&mut self, _cluster: XtreamCluster) { /* noop */ }
    fn obtain_resources(&mut self) -> BoxFuture<'_, ()> { Box::pin(async move {}) }
    fn sort_by_provider_ordinal(&mut self) { /* noop */ }
}

pub struct XtreamDiskPlaylistSource {
    app_config: Arc<AppConfig>,
    storage_path: PathBuf,
    live: Option<XtreamQueryHandle>,
    vod: Option<XtreamQueryHandle>,
    series: Option<XtreamQueryHandle>,
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

impl PlaylistSourceOps for XtreamDiskPlaylistSource {
    fn is_memory(&self) -> bool { false }

    fn get_channel_count(&mut self) -> usize {
        self.live.as_mut().map_or(0usize, |(t, _)| t.len().unwrap_or(0usize))
            + self.vod.as_mut().map_or(0usize, |(t, _)| t.len().unwrap_or(0usize))
            + self.series.as_mut().map_or(0usize, |(t, _)| t.len().unwrap_or(0usize))
    }

    fn get_group_count(&mut self) -> usize {
        fn collect_groups<Q>(query: &mut Option<(BPlusTreeQuery<u32, XtreamPlaylistItem>, Q)>, groups: &mut HashSet<Arc<str>>) {
            if let Some((query, _)) = query { for (_, item) in query.iter() { groups.insert(item.group.clone()); } }
        }
        let mut groups = HashSet::new();

        collect_groups(&mut self.live, &mut groups);
        collect_groups(&mut self.vod, &mut groups);
        collect_groups(&mut self.series, &mut groups);
        groups.len()
    }

    fn get_channel_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
        let live_count = if skip_set.contains(&XtreamCluster::Live) {
            0
        } else {
            self.live.as_mut().map_or(0usize, |(query, _)| query.len().unwrap_or(0usize))
        };
        let vod_count = if skip_set.contains(&XtreamCluster::Video) {
            0
        } else {
            self.vod.as_mut().map_or(0usize, |(query, _)| query.len().unwrap_or(0usize))
        };
        let series_count = if skip_set.contains(&XtreamCluster::Series) {
            0
        } else {
            self.series.as_mut().map_or(0usize, |(query, _)| query.len().unwrap_or(0usize))
        };
        live_count + vod_count + series_count
    }

    fn get_group_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
        fn collect_groups<Q>(
            cluster: XtreamCluster,
            query: &mut Option<(BPlusTreeQuery<u32, XtreamPlaylistItem>, Q)>,
            groups: &mut HashSet<(XtreamCluster, Arc<str>)>,
            skip_set: &HashSet<XtreamCluster>,
        ) {
            if skip_set.contains(&cluster) {
                return;
            }
            if let Some((query, _)) = query {
                for (_, item) in query.iter() {
                    groups.insert((cluster, Arc::clone(&item.group)));
                }
            }
        }

        let mut groups = HashSet::new();
        collect_groups(XtreamCluster::Live, &mut self.live, &mut groups, skip_set);
        collect_groups(XtreamCluster::Video, &mut self.vod, &mut groups, skip_set);
        collect_groups(XtreamCluster::Series, &mut self.series, &mut groups, skip_set);
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
        self.vod.as_mut().map_or(0, |(query, _)| {
            query.iter().filter(|(_, item)| {
                item.item_type == PlaylistItemType::Video && item.provider_id > 0 && !item.has_details()
            }).count()
        })
    }

    fn get_missing_series_info_count(&mut self) -> usize {
        self.series.as_mut().map_or(0, |(query, _)| {
            query.iter().filter(|(_, item)| {
                item.item_type == PlaylistItemType::SeriesInfo && item.provider_id > 0 && !item.has_details()
            }).count()
        })
    }

    fn get_missing_vod_info_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
        if skip_set.contains(&XtreamCluster::Video) {
            0
        } else {
            self.get_missing_vod_info_count()
        }
    }

    fn get_missing_series_info_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
        if skip_set.contains(&XtreamCluster::Series) {
            0
        } else {
            self.get_missing_series_info_count()
        }
    }

    fn deduplicate(&mut self, _duplicates: &mut HashSet<UUIDType>) {
        warn!("Deduplication is not supported for disk based playlist updates");
    }

    fn take_groups(&mut self) -> Vec<PlaylistGroup> {
        // Build groups on-the-fly using disk iterator (streams one leaf at a time)
        let mut groups_map: IndexMap<(XtreamCluster, u32), PlaylistGroup> = IndexMap::new();
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
                groups_map.entry((cluster, item.category_id))
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
            group
                .channels
                .sort_by_key(|item| normalized_source_ordinal(item.header.source_ordinal));
        }

        // Sort groups based on the source_ordinal of their first channel
        let mut groups: Vec<PlaylistGroup> = groups_map.into_values().collect();
        groups.sort_by_key(|group| group.channels.first().map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal)));
        groups
    }

    fn clone_source(&self) -> Result<PlaylistSource, TuliproxError> {
        let live = clone_xtream_query("live", self.live.as_ref())?;
        let vod = clone_xtream_query("vod", self.vod.as_ref())?;
        let series = clone_xtream_query("series", self.series.as_ref())?;

        Ok(PlaylistSource::xtream_disk(Self {
            app_config: Arc::clone(&self.app_config),
            storage_path: self.storage_path.clone(),
            live,
            vod,
            series,
        }))
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
    ($name:ident, $key_type:ty, $entry_type:ty) => {
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

        impl PlaylistSourceOps for [<$name DiskPlaylistSource>] {

            fn get_channel_count(&mut self) -> usize { self.playlist.as_mut().map_or(0usize, |t: &mut BPlusTreeQuery<$key_type, $entry_type>| t.len().unwrap_or(0usize)) }

            fn is_memory(&self) -> bool { false }

            fn get_group_count(&mut self) -> usize {
                let mut groups = HashSet::new();
                if let Some(query) = self.playlist.as_mut() { for (_, item) in query.iter() { groups.insert(item.group.clone()); } }
                groups.len()
            }

            fn get_channel_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
                self.playlist.as_mut().map_or(0, |query| {
                    query
                        .iter()
                        .filter(|(_, item)| !skip_set.contains(&cluster_from_item_type(item.item_type)))
                        .count()
                })
            }

            fn get_group_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
                let mut groups = HashSet::<(XtreamCluster, Arc<str>)>::new();
                if let Some(query) = self.playlist.as_mut() {
                    for (_, item) in query.iter() {
                        let cluster = cluster_from_item_type(item.item_type);
                        if !skip_set.contains(&cluster) {
                            groups.insert((cluster, Arc::clone(&item.group)));
                        }
                    }
                }
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
                    warn!("update_playlist should not be called for Disk playlist");
                })
            }

            fn get_missing_vod_info_count(&mut self) -> usize { 0 }
            fn get_missing_series_info_count(&mut self) -> usize { 0 }
            fn get_missing_vod_info_count_excluding_clusters(&mut self, _skip_set: &HashSet<XtreamCluster>) -> usize { 0 }
            fn get_missing_series_info_count_excluding_clusters(&mut self, _skip_set: &HashSet<XtreamCluster>) -> usize { 0 }
            fn deduplicate(&mut self, _duplicates: &mut HashSet<UUIDType>) {
                warn!("Deduplication is not supported for disk based playlist updates");
            }
            fn take_groups(&mut self) -> Vec<PlaylistGroup> {
                // Build groups on-the-fly using disk iterator (streams one leaf at a time)
                if let Some(q) = self.playlist.as_mut() {
                    let mut groups_map: IndexMap<(XtreamCluster, Arc<str>), PlaylistGroup> = IndexMap::new();
                    for (_, item) in q.iter() {
                        let cluster = XtreamCluster::try_from(item.item_type).unwrap_or(XtreamCluster::Live);
                        let normalized_group = shared::utils::deunicode_string(&item.group).to_lowercase().intern();
                        let key = (cluster, normalized_group);
                        groups_map.entry(key)
                            .or_insert_with(|| PlaylistGroup {
                                id: 0,
                                title: item.group.clone(),
                                channels: vec![],
                                xtream_cluster: cluster,
                            })
                            .channels.push(PlaylistItem::from(&item));
                    }
                    // Sort channels within each group
                    for group in groups_map.values_mut() {
                        group
                            .channels
                            .sort_by_key(|item| normalized_source_ordinal(item.header.source_ordinal));
                    }

                    // Sort groups based on the source_ordinal of their first channel
                    let mut groups: Vec<PlaylistGroup> = groups_map.into_values().collect();
                    groups.sort_by_key(|group| group.channels.first().map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal)));
                    groups
                } else {
                    vec![]
                }
            }
            fn clone_source(&self) -> Result<PlaylistSource, TuliproxError> {
                let playlist = self
                    .playlist
                    .as_ref()
                    .map(|query| {
                        query.try_clone().map_err(|err| {
                            TuliproxError::RepositoryPlaylist(format!(
                                "Failed to clone {} disk playlist query {}: {err}",
                                stringify!($name),
                                self.file_path.display()
                            ))
                        })
                    })
                    .transpose()?;

                Ok(PlaylistSource::[<$name:snake _disk>](Self {
                    app_config: Arc::clone(&self.app_config),
                    file_path: self.file_path.clone(),
                    playlist,
                    guard: self.guard.clone(),
                }))
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

impl_single_file_disk_source!(M3u, Arc<str>, M3uPlaylistItem);

impl_single_file_disk_source!(LocalLibrary, UUIDType, XtreamPlaylistItem);
impl_single_file_disk_source!(MediaServer, UUIDType, XtreamPlaylistItem);

pub struct MemoryPlaylistSource {
    playlist: Arc<Vec<PlaylistGroup>>,
}

impl MemoryPlaylistSource {
    pub(crate) fn new(groups: Vec<PlaylistGroup>) -> Self {
        Self { playlist: Arc::new(groups) }
    }

    pub fn into_source(self) -> PlaylistSource { PlaylistSource::memory(self) }

    /// Merge a batch of groups into the in-memory playlist in a single pass.
    ///
    /// Equivalent to calling [`PlaylistSourceOps::update_playlist`] for every
    /// incoming group, but builds `(cluster, normalized_title)` and
    /// `(cluster, id)` indexes over the existing groups once instead of
    /// re-scanning (and re-normalizing every existing title) for each incoming
    /// group. This turns the series-expansion merge from O(groups^2) into a
    /// single linear pass. Title matches take precedence over id matches, and
    /// newly pushed groups are indexed so later groups in the same batch can
    /// merge into them, preserving the sequential semantics.
    pub(crate) fn merge_groups(&mut self, incoming: Vec<PlaylistGroup>) {
        if incoming.is_empty() {
            return;
        }
        let playlist = Arc::make_mut(&mut self.playlist);
        let mut by_title: HashMap<(XtreamCluster, String), usize> = HashMap::with_capacity(playlist.len());
        let mut by_id: HashMap<(XtreamCluster, u32), usize> = HashMap::with_capacity(playlist.len());
        for (idx, grp) in playlist.iter().enumerate() {
            by_title
                .entry((grp.xtream_cluster, shared::utils::deunicode_string(&grp.title).to_lowercase()))
                .or_insert(idx);
            by_id.entry((grp.xtream_cluster, grp.id)).or_insert(idx);
        }
        for plg in incoming {
            let title_key = (plg.xtream_cluster, shared::utils::deunicode_string(&plg.title).to_lowercase());
            if let Some(&idx) = by_title.get(&title_key) {
                playlist[idx].channels.extend(plg.channels);
                continue;
            }
            if let Some(&idx) = by_id.get(&(plg.xtream_cluster, plg.id)) {
                playlist[idx].channels.extend(plg.channels);
                continue;
            }
            let new_idx = playlist.len();
            by_title.entry(title_key).or_insert(new_idx);
            by_id.entry((plg.xtream_cluster, plg.id)).or_insert(new_idx);
            playlist.push(plg);
        }
    }
}

impl Default for MemoryPlaylistSource {
    fn default() -> Self {
        Self { playlist: Arc::new(vec![]) }
    }
}

impl PlaylistSourceOps for MemoryPlaylistSource {
    fn is_memory(&self) -> bool { true }
    fn get_channel_count(&mut self) -> usize { self.playlist.iter().map(|group| group.channels.len()).sum() }
    fn get_group_count(&mut self) -> usize { self.playlist.len() }
    fn get_channel_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
        self.playlist
            .iter()
            .filter(|group| !skip_set.contains(&group.xtream_cluster))
            .map(|group| {
                group
                    .channels
                    .iter()
                    .filter(|item| !skip_set.contains(&item.header.xtream_cluster))
                    .count()
            })
            .sum()
    }
    fn get_group_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
        self.playlist
            .iter()
            .filter(|group| !skip_set.contains(&group.xtream_cluster))
            .filter(|group| group.channels.iter().any(|item| !skip_set.contains(&item.header.xtream_cluster)))
            .count()
    }
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
            let incoming_title = shared::utils::deunicode_string(&plg.title).to_lowercase();
            for grp in playlist.iter_mut() {
                let existing_title = shared::utils::deunicode_string(&grp.title).to_lowercase();
                if grp.xtream_cluster == plg.xtream_cluster && existing_title == incoming_title {
                    grp.channels.extend(plg.channels.iter().cloned());
                    return;
                }
            }
            for grp in playlist.iter_mut() {
                if grp.xtream_cluster == plg.xtream_cluster && grp.id == plg.id {
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
    fn get_missing_vod_info_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
        if skip_set.contains(&XtreamCluster::Video) {
            0
        } else {
            self.get_missing_vod_info_count()
        }
    }
    fn get_missing_series_info_count_excluding_clusters(&mut self, skip_set: &HashSet<XtreamCluster>) -> usize {
        if skip_set.contains(&XtreamCluster::Series) {
            0
        } else {
            self.get_missing_series_info_count()
        }
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
    fn clone_source(&self) -> Result<PlaylistSource, TuliproxError> {
        Ok(PlaylistSource::memory(MemoryPlaylistSource { playlist: Arc::clone(&self.playlist) }))
    }
    fn release_resources(&mut self, _cluster: XtreamCluster) { /* noop */ }
    fn obtain_resources(&mut self) -> BoxFuture<'_, ()> { Box::pin(async move {}) }

    fn sort_by_provider_ordinal(&mut self) {
        let playlist = Arc::make_mut(&mut self.playlist);
        for group in &mut *playlist {
            group
                .channels
                .sort_by_key(|item| normalized_source_ordinal(item.header.source_ordinal));
        }
        playlist.sort_by_key(|group| group.channels.first().map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal)));
    }
}

async fn load_bplustree_query<K, P>(app_config: &Arc<AppConfig>, file_path: &Path) -> Option<(BPlusTreeQuery<K, P>, FileReadGuard)>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone + Send + 'static,
    P: Serialize + for<'de> Deserialize<'de> + Clone + Send + 'static,
{
    if file_path.exists() {
        let guard = app_config.file_locks.read_lock(file_path).await;
        let file_path = file_path.to_path_buf();
        let file_path_err = file_path.clone();
        match tokio::task::spawn_blocking(move || {
            BPlusTreeQuery::<K, P>::try_new(&file_path).map(|query| (query, guard))
        })
        .await
        {
            Ok(Ok((query, guard))) => Some((query, guard)),
            Ok(Err(err)) => {
                error!(
                    "I/O error loading disk playlist {} via BPlusTreeQuery::try_new: {err}",
                    file_path_err.display()
                );
                None
            }
            Err(err) => {
                error!(
                    "Task/JoinError loading disk playlist {}: {err} (panic={}, cancelled={})",
                    file_path_err.display(),
                    err.is_panic(),
                    err.is_cancelled()
                );
                None
            }
        }
    } else { None }
}

#[cfg(test)]
mod tests {
    use super::{MemoryPlaylistSource, PlaylistGroup, PlaylistItem, PlaylistSource, XtreamCluster, XtreamDiskPlaylistSource};
    use crate::model::{AppConfig, Config, MediaToolCapabilities, SourcesConfig};
    use crate::repository::BPlusTreeQuery;
    use crate::utils::FileLockManager;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::error::TuliproxError;
    use shared::model::{ConfigPaths, PlaylistItemHeader, PlaylistItemType, XtreamPlaylistItem};
    use shared::utils::Internable;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_app_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig {
                batch_files: Vec::new(),
                templates: None,
                provider: Vec::new(),
                inputs: Vec::new(),
                sources: Vec::new(),
            })),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::empty()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::empty()),
            access_token_secret: [0u8; 32],
            encrypt_secret: [0u8; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        })
    }

    fn make_item(title: &str, group: &str, category_id: u32) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: title.intern(),
                title: title.intern(),
                group: group.intern(),
                category_id,
                xtream_cluster: XtreamCluster::Series,
                item_type: PlaylistItemType::SeriesInfo,
                url: format!("http://example.test/{title}").intern(),
                input_name: "test-input".intern(),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn update_playlist_prefers_group_title_over_colliding_group_id() {
        let first_group = PlaylistGroup {
            id: 1,
            title: "A-First".intern(),
            channels: vec![make_item("first-item", "A-First", 11)],
            xtream_cluster: XtreamCluster::Series,
        };
        let target_group = PlaylistGroup {
            id: 2,
            title: "B-Series".intern(),
            channels: vec![make_item("target-item", "B-Series", 22)],
            xtream_cluster: XtreamCluster::Series,
        };
        let mut source = MemoryPlaylistSource::new(vec![first_group, target_group]).into_source();

        // Simulates a mapped delta group whose local pipeline id restarted at 1.
        let incoming = PlaylistGroup {
            id: 1,
            title: "B-Series".intern(),
            channels: vec![make_item("new-episode", "B-Series", 22)],
            xtream_cluster: XtreamCluster::Series,
        };
        source.update_playlist(&incoming).await;

        let groups = source.take_groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title.as_ref(), "A-First");
        assert_eq!(groups[0].channels.len(), 1);
        assert_eq!(groups[1].title.as_ref(), "B-Series");
        assert_eq!(groups[1].channels.len(), 2);
    }

    #[tokio::test]
    async fn cloned_memory_source_is_copy_on_write() {
        let group = PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![make_item("original", "Series", 1)],
            xtream_cluster: XtreamCluster::Series,
        };
        let source = MemoryPlaylistSource::new(vec![group]).into_source();
        let mut original = source.clone_source().expect("memory source clone should succeed");
        let mut cloned = source.clone_source().expect("memory source clone should succeed");

        let incoming = PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![make_item("new", "Series", 1)],
            xtream_cluster: XtreamCluster::Series,
        };
        cloned.update_playlist(&incoming).await;

        assert_eq!(original.get_channel_count(), 1);
        assert_eq!(cloned.get_channel_count(), 2);
    }

    #[tokio::test]
    async fn xtream_disk_clone_source_returns_error_when_query_clone_fails() {
        let app_config = test_app_config();
        let storage_path = PathBuf::from("clone-error-fixture");
        let guard = Arc::new(app_config.file_locks.read_lock(&storage_path).await);
        let source = PlaylistSource::xtream_disk(XtreamDiskPlaylistSource {
            app_config,
            storage_path,
            live: Some((BPlusTreeQuery::<u32, XtreamPlaylistItem>::clone_error_fixture(), guard)),
            vod: None,
            series: None,
        });

        let result = source.clone_source();

        assert!(matches!(
            result,
            Err(TuliproxError::RepositoryPlaylist(message))
                if message.contains("Failed to clone live disk playlist query")
                    && message.contains("No data source available to clone")
        ));
    }

    fn make_cluster_item(title: &str, cluster: XtreamCluster) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                id: title.intern(),
                title: title.intern(),
                group: format!("{cluster:?}").intern(),
                xtream_cluster: cluster,
                item_type: PlaylistItemType::from(cluster),
                url: format!("http://example.test/{title}").intern(),
                input_name: "test-input".intern(),
                ..Default::default()
            },
        }
    }

    fn make_cluster_group(title: &str, cluster: XtreamCluster, channels: Vec<PlaylistItem>) -> PlaylistGroup {
        PlaylistGroup {
            id: cluster as u32,
            title: title.intern(),
            channels,
            xtream_cluster: cluster,
        }
    }

    fn filtered_source(skip_cluster: XtreamCluster) -> PlaylistSource {
        let groups = vec![
            make_cluster_group("Live", XtreamCluster::Live, vec![make_cluster_item("live-1", XtreamCluster::Live)]),
            make_cluster_group(
                "Video",
                XtreamCluster::Video,
                vec![make_cluster_item("video-1", XtreamCluster::Video)],
            ),
            make_cluster_group(
                "Series",
                XtreamCluster::Series,
                vec![make_cluster_item("series-1", XtreamCluster::Series)],
            ),
        ];
        PlaylistSource::filtered(
            MemoryPlaylistSource::new(groups).into_source(),
            HashSet::from([skip_cluster]),
        )
    }

    #[test]
    fn filtered_source_excludes_skipped_cluster_from_counts_and_items() {
        let mut source = filtered_source(XtreamCluster::Video);

        assert_eq!(source.get_channel_count(), 2);
        assert_eq!(source.get_group_count(), 2);
        assert_eq!(
            source
                .items()
                .map(|item| item.as_ref().header.xtream_cluster)
                .collect::<Vec<_>>(),
            vec![XtreamCluster::Live, XtreamCluster::Series]
        );
    }

    #[test]
    fn filtered_source_excludes_skipped_cluster_when_taking_groups() {
        let mut source = filtered_source(XtreamCluster::Video);

        let groups = source.take_groups();

        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|group| group.xtream_cluster != XtreamCluster::Video));
    }

    #[test]
    fn filtered_source_deduplicates_visible_memory_items_only() {
        let duplicated_live = make_cluster_item("same-live", XtreamCluster::Live);
        let groups = vec![
            make_cluster_group(
                "Live",
                XtreamCluster::Live,
                vec![duplicated_live.clone(), duplicated_live],
            ),
            make_cluster_group(
                "Video",
                XtreamCluster::Video,
                vec![make_cluster_item("video-1", XtreamCluster::Video)],
            ),
        ];
        let mut source = PlaylistSource::filtered(
            MemoryPlaylistSource::new(groups).into_source(),
            HashSet::from([XtreamCluster::Video]),
        );
        let mut duplicates = HashSet::new();

        source.deduplicate(&mut duplicates);

        let groups = source.take_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].channels.len(), 1);
        assert_eq!(groups[0].xtream_cluster, XtreamCluster::Live);
    }
}
