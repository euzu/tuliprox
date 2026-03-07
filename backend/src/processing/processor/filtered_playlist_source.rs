use crate::repository::{MemoryPlaylistSource, PlaylistSource};
use futures::future::BoxFuture;
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemType, UUIDType, XtreamCluster};
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) struct FilteredPlaylistSource {
    inner: Box<dyn PlaylistSource>,
    skip_set: Arc<HashSet<XtreamCluster>>,
}

impl FilteredPlaylistSource {
    pub(crate) fn new(inner: Box<dyn PlaylistSource>, skip_set: HashSet<XtreamCluster>) -> Self {
        Self {
            inner,
            skip_set: Arc::new(skip_set),
        }
    }

    fn filter_group(&self, mut group: PlaylistGroup) -> Option<PlaylistGroup> {
        if self.skip_set.contains(&group.xtream_cluster) {
            return None;
        }
        group.channels.retain(|item| !self.skip_set.contains(&item.header.xtream_cluster));
        if group.channels.is_empty() {
            None
        } else {
            Some(group)
        }
    }
}

impl PlaylistSource for FilteredPlaylistSource {
    fn is_memory(&self) -> bool {
        self.inner.is_memory()
    }

    fn get_channel_count(&mut self) -> usize {
        let skip_set = Arc::clone(&self.skip_set);
        self.inner
            .items()
            .filter(move |item| !skip_set.contains(&item.as_ref().header.xtream_cluster))
            .count()
    }

    fn get_group_count(&mut self) -> usize {
        let skip_set = Arc::clone(&self.skip_set);
        let mut groups = HashSet::<(XtreamCluster, Arc<str>)>::new();
        for item in self
            .inner
            .items()
            .filter(move |item| !skip_set.contains(&item.as_ref().header.xtream_cluster))
        {
            let pli = item.as_ref();
            groups.insert((pli.header.xtream_cluster, Arc::clone(&pli.header.group)));
        }
        groups.len()
    }

    fn is_empty(&mut self) -> bool {
        self.get_channel_count() == 0
    }

    #[allow(clippy::wrong_self_convention)]
    fn into_items(&mut self) -> Box<dyn Iterator<Item = PlaylistItem> + Send + '_> {
        let skip_set = Arc::clone(&self.skip_set);
        Box::new(
            self.inner
                .into_items()
                .filter(move |item| !skip_set.contains(&item.header.xtream_cluster)),
        )
    }

    fn items_mut(&mut self) -> Box<dyn Iterator<Item = &mut PlaylistItem> + Send + '_> {
        let skip_set = Arc::clone(&self.skip_set);
        Box::new(self.inner.items_mut().filter_map(move |item| {
            if skip_set.contains(&item.header.xtream_cluster) {
                None
            } else {
                Some(item)
            }
        }))
    }

    fn items<'a>(&'a mut self) -> Box<dyn Iterator<Item = std::borrow::Cow<'a, PlaylistItem>> + Send + 'a> {
        let skip_set = Arc::clone(&self.skip_set);
        Box::new(
            self.inner
                .items()
                .filter(move |item| !skip_set.contains(&item.as_ref().header.xtream_cluster)),
        )
    }

    fn update_playlist<'a>(&'a mut self, plg: &'a PlaylistGroup) -> BoxFuture<'a, ()> {
        if self.skip_set.contains(&plg.xtream_cluster) {
            return Box::pin(async move {});
        }
        self.inner.update_playlist(plg)
    }

    fn get_missing_vod_info_count(&mut self) -> usize {
        let skip_set = Arc::clone(&self.skip_set);
        self.inner
            .items()
            .filter(move |item| !skip_set.contains(&item.as_ref().header.xtream_cluster))
            .filter(|item| {
                let pli = item.as_ref();
                pli.header.xtream_cluster == XtreamCluster::Video
                    && pli.header.item_type == PlaylistItemType::Video
                    && !pli.has_details()
            })
            .count()
    }

    fn get_missing_series_info_count(&mut self) -> usize {
        let skip_set = Arc::clone(&self.skip_set);
        self.inner
            .items()
            .filter(move |item| !skip_set.contains(&item.as_ref().header.xtream_cluster))
            .filter(|item| {
                let pli = item.as_ref();
                pli.header.xtream_cluster == XtreamCluster::Series
                    && pli.header.item_type == PlaylistItemType::SeriesInfo
                    && pli.header.id.parse::<u32>().is_ok_and(|id| id > 0)
                    && !pli.has_details()
            })
            .count()
    }

    fn deduplicate(&mut self, duplicates: &mut HashSet<UUIDType>) {
        if self.inner.is_memory() {
            let filtered_groups = self
                .inner
                .take_groups()
                .into_iter()
                .filter_map(|group| self.filter_group(group))
                .collect::<Vec<_>>();
            let mut memory = MemoryPlaylistSource::new(filtered_groups).boxed();
            memory.deduplicate(duplicates);
            self.inner = memory;
            return;
        }

        self.inner.deduplicate(duplicates);
    }

    fn take_groups(&mut self) -> Vec<PlaylistGroup> {
        self.inner
            .take_groups()
            .into_iter()
            .filter_map(|group| self.filter_group(group))
            .collect()
    }

    fn clone_box(&self) -> Box<dyn PlaylistSource> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            skip_set: Arc::clone(&self.skip_set),
        })
    }

    fn release_resources(&mut self, cluster: XtreamCluster) {
        self.inner.release_resources(cluster);
    }

    fn obtain_resources(&mut self) -> BoxFuture<'_, ()> {
        self.inner.obtain_resources()
    }

    fn sort_by_provider_ordinal(&mut self) {
        self.inner.sort_by_provider_ordinal();
    }
}
