use super::{BPlusTree, VirtualIdRecord};
use shared::model::{M3uPlaylistItem, PlaylistItem, PlaylistItemType, VirtualId, XtreamCluster, XtreamPlaylistItem};
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::RwLock;
use tuliprox_core::model::ConfigTarget;

#[inline]
pub(crate) fn is_cluster_order_eligible(cluster: XtreamCluster, item_type: PlaylistItemType) -> bool {
    match cluster {
        XtreamCluster::Series => item_type.is_series_info(),
        XtreamCluster::Live | XtreamCluster::Video => true,
    }
}

pub struct PlaylistXtreamStorage {
    pub live: BPlusTree<u32, XtreamPlaylistItem>,
    pub live_order: Vec<(u32, u32)>,
    pub vod: BPlusTree<u32, XtreamPlaylistItem>,
    pub vod_order: Vec<(u32, u32)>,
    pub series: BPlusTree<u32, XtreamPlaylistItem>,
    pub series_order: Vec<(u32, u32)>,
}

impl Default for PlaylistXtreamStorage {
    fn default() -> Self { Self::new() }
}

impl PlaylistXtreamStorage {
    pub fn new() -> Self {
        Self {
            live: BPlusTree::new(),
            live_order: Vec::new(),
            vod: BPlusTree::new(),
            vod_order: Vec::new(),
            series: BPlusTree::new(),
            series_order: Vec::new(),
        }
    }

    pub fn from_trees(
        live: BPlusTree<u32, XtreamPlaylistItem>,
        vod: BPlusTree<u32, XtreamPlaylistItem>,
        series: BPlusTree<u32, XtreamPlaylistItem>,
    ) -> Self {
        let live_order = Self::build_cluster_order(XtreamCluster::Live, &live);
        let vod_order = Self::build_cluster_order(XtreamCluster::Video, &vod);
        let series_order = Self::build_cluster_order(XtreamCluster::Series, &series);
        Self { live, live_order, vod, vod_order, series, series_order }
    }

    pub(crate) fn build_cluster_order(
        cluster: XtreamCluster,
        tree: &BPlusTree<u32, XtreamPlaylistItem>,
    ) -> Vec<(u32, u32)> {
        let mut order: Vec<(u32, u32)> = tree
            .iter()
            .filter(|(_, item)| is_cluster_order_eligible(cluster, item.item_type))
            .map(|(key, item)| (item.source_ordinal, *key))
            .collect();
        order.sort_unstable();
        order
    }

    #[inline]
    pub fn get_cluster(&self, cluster: XtreamCluster) -> (&BPlusTree<u32, XtreamPlaylistItem>, &[(u32, u32)]) {
        match cluster {
            XtreamCluster::Live => (&self.live, &self.live_order),
            XtreamCluster::Video => (&self.vod, &self.vod_order),
            XtreamCluster::Series => (&self.series, &self.series_order),
        }
    }

    #[inline]
    fn get_cluster_mut(
        &mut self,
        cluster: XtreamCluster,
    ) -> (&mut BPlusTree<u32, XtreamPlaylistItem>, &mut Vec<(u32, u32)>) {
        match cluster {
            XtreamCluster::Live => (&mut self.live, &mut self.live_order),
            XtreamCluster::Video => (&mut self.vod, &mut self.vod_order),
            XtreamCluster::Series => (&mut self.series, &mut self.series_order),
        }
    }

    /// Inserts or replaces one item. New keys are binary-search inserted into the cluster order,
    /// and repositioned when `source_ordinal` changes. Series items are stored without season/episode details;
    /// lookups that need them read the item from disk. In the Series cluster only `SeriesInfo`/`LocalSeriesInfo`
    /// enter the order.
    pub fn upsert(&mut self, cluster: XtreamCluster, mut item: XtreamPlaylistItem) {
        if cluster == XtreamCluster::Series {
            item.clear_series_details();
        }
        let key = item.virtual_id.get();
        let new_ordinal = item.source_ordinal;
        let new_eligible = is_cluster_order_eligible(cluster, item.item_type);

        let (tree, order) = self.get_cluster_mut(cluster);
        let mut already_in_place = false;
        if let Some(old) = tree.query(&key) {
            let old_ordinal = old.source_ordinal;
            let old_eligible = is_cluster_order_eligible(cluster, old.item_type);
            if old_eligible {
                if new_eligible && old_ordinal == new_ordinal {
                    already_in_place = true;
                } else if let Ok(pos) = order.binary_search(&(old_ordinal, key)) {
                    order.remove(pos);
                }
            }
        }
        if new_eligible && !already_in_place {
            let pos = order.binary_search(&(new_ordinal, key)).unwrap_or_else(|pos| pos);
            order.insert(pos, (new_ordinal, key));
        }
        tree.insert(key, item);
    }
}

pub type PlaylistM3uStorage = BPlusTree<u32, M3uPlaylistItem>;

pub enum PlaylistStorage {
    M3uPlaylist(Box<PlaylistM3uStorage>),
    XtreamPlaylist(Box<PlaylistXtreamStorage>),
}

pub struct TargetPlaylistStorage {
    pub xtream: Option<PlaylistXtreamStorage>,
    pub m3u: Option<PlaylistM3uStorage>,
    pub id_mapping: Option<BPlusTree<VirtualId, VirtualIdRecord>>,
}

pub type TargetPlaylistStorageMap = HashMap<String, TargetPlaylistStorage>;

pub struct PlaylistStorageState {
    pub data: RwLock<TargetPlaylistStorageMap>,
    /// Generation of the persisted failover groups per target name; a missing entry is generation 0.
    failover_group_generations: Mutex<HashMap<String, u64>>,
    pub failover_groups: crate::FailoverGroupCache,
}

impl Default for PlaylistStorageState {
    fn default() -> Self { Self::new() }
}

impl PlaylistStorageState {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
            failover_group_generations: Mutex::new(HashMap::new()),
            failover_groups: crate::FailoverGroupCache::new(),
        }
    }

    pub fn bump_failover_group_generation(&self, target_name: &str) -> u64 {
        let mut generations = self.failover_group_generations.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = generations.entry(target_name.to_string()).or_insert(0);
        *generation += 1;
        *generation
    }

    #[must_use]
    pub fn failover_group_generation(&self, target_name: &str) -> u64 {
        let generations = self.failover_group_generations.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        generations.get(target_name).copied().unwrap_or(0)
    }

    /// Replaces one target only after its complete persisted state has been loaded.
    pub(crate) async fn replace_target(&self, target_name: &str, storage: TargetPlaylistStorage) {
        self.data.write().await.insert(target_name.to_string(), storage);
    }

    pub async fn update_target_id_mapping(&self, target: &ConfigTarget, mapping: Vec<VirtualIdRecord>) {
        if target.use_memory_cache {
            if let Some(storage) = self.data.write().await.get_mut(&target.name) {
                if let Some(id_mapping) = storage.id_mapping.as_mut() {
                    for record in mapping {
                        id_mapping.insert(record.virtual_id, record);
                    }
                }
            }
        }
    }

    pub async fn update_playlist_items(&self, target: &ConfigTarget, pli_list: &[&XtreamPlaylistItem]) {
        if target.use_memory_cache {
            if let Some(storage) = self.data.write().await.get_mut(&target.name) {
                if let Some(xtream) = storage.xtream.as_mut() {
                    for &pli in pli_list {
                        xtream.upsert(pli.xtream_cluster, pli.clone());
                    }
                }
            }
        }
    }

    pub async fn insert_playlist_items(&self, target: &ConfigTarget, pli_list: Vec<PlaylistItem>) {
        if target.use_memory_cache {
            if let Some(storage) = self.data.write().await.get_mut(&target.name) {
                if let Some(xtream) = storage.xtream.as_mut() {
                    for pli in pli_list {
                        xtream.upsert(pli.header.xtream_cluster, XtreamPlaylistItem::from(&pli));
                    }
                }
            }
        }
    }

    pub async fn cache_id_mapping(&self, target_name: &str, id_mapping: BPlusTree<VirtualId, VirtualIdRecord>) {
        match self.data.write().await.entry(target_name.to_string()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let storage = entry.get_mut();
                storage.id_mapping = Some(id_mapping);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(TargetPlaylistStorage { xtream: None, m3u: None, id_mapping: Some(id_mapping) });
            }
        }
    }

    pub async fn cache_playlist(&self, target_name: &str, playlist: PlaylistStorage) {
        match playlist {
            PlaylistStorage::M3uPlaylist(m3u_playlist) => {
                match self.data.write().await.entry(target_name.to_string()) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        let storage = entry.get_mut();
                        storage.m3u = Some(*m3u_playlist);
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(TargetPlaylistStorage {
                            xtream: None,
                            m3u: Some(*m3u_playlist),
                            id_mapping: None,
                        });
                    }
                }
            }
            PlaylistStorage::XtreamPlaylist(xtream_playlist) => {
                match self.data.write().await.entry(target_name.to_string()) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        let storage = entry.get_mut();
                        storage.xtream = Some(*xtream_playlist);
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(TargetPlaylistStorage {
                            xtream: Some(*xtream_playlist),
                            m3u: None,
                            id_mapping: None,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{
        model::{PlaylistItemHeader, PlaylistItemType, SeriesStreamDetailProperties, StreamProperties, VirtualId},
        utils::Internable,
    };
    use tuliprox_core::model::{ConfigTarget, TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput};

    fn sample_target(use_memory_cache: bool) -> ConfigTarget {
        let mut target = crate::test_config::target_with_output(
            "test-target",
            TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None }),
        );
        target.use_memory_cache = use_memory_cache;
        target
    }

    fn sample_xtream_item(id: u32, cluster: XtreamCluster) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: VirtualId::new(id),
            provider_id: id,
            name: format!("Item {id}").intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "".intern(),
            epg_channel_id: None,
            xtream_cluster: cluster,
            additional_properties: None,
            item_type: match cluster {
                XtreamCluster::Live => PlaylistItemType::Live,
                XtreamCluster::Video => PlaylistItemType::Video,
                XtreamCluster::Series => PlaylistItemType::SeriesInfo,
            },
            category_id: 1,
            input_name: "test".intern(),
            channel_no: id,
            source_ordinal: id,
            input_stream_id: id.to_string().intern(),
            upstream_user_agent: None,
        }
    }

    #[tokio::test]
    async fn update_playlist_items_appends_only_new_keys_to_order() {
        let playlists = PlaylistStorageState::new();
        let target = sample_target(true);

        // Prepopulate storage with items 10 and 20
        let mut storage = PlaylistXtreamStorage::new();
        let item10 = sample_xtream_item(10, XtreamCluster::Live);
        let item20 = sample_xtream_item(20, XtreamCluster::Live);
        storage.live.insert(10, item10.clone());
        storage.live.insert(20, item20.clone());
        storage.live_order = vec![(10, 10), (20, 20)];

        playlists.cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::new(storage))).await;

        // 1. Update existing key 20 with same source_ordinal: order must NOT duplicate 20
        playlists.update_playlist_items(&target, &[&item20]).await;
        {
            let data = playlists.data.read().await;
            let xtream = data.get(&target.name).unwrap().xtream.as_ref().unwrap();
            assert_eq!(xtream.live_order, vec![(10, 10), (20, 20)]);
        }

        // 2. Add new key 30 via update_playlist_items: order must binary-search insert 30
        let item30 = sample_xtream_item(30, XtreamCluster::Live);
        playlists.update_playlist_items(&target, &[&item30]).await;
        {
            let data = playlists.data.read().await;
            let xtream = data.get(&target.name).unwrap().xtream.as_ref().unwrap();
            assert_eq!(xtream.live_order, vec![(10, 10), (20, 20), (30, 30)]);
        }

        // 3. Update existing key 20 with changed source_ordinal 5: repositions before 10
        let mut item20_moved = item20.clone();
        item20_moved.source_ordinal = 5;
        playlists.update_playlist_items(&target, &[&item20_moved]).await;
        {
            let data = playlists.data.read().await;
            let xtream = data.get(&target.name).unwrap().xtream.as_ref().unwrap();
            assert_eq!(xtream.live_order, vec![(5, 20), (10, 10), (30, 30)]);
        }

        // 4. Add existing key 10 via insert_playlist_items: order unchanged
        let pli10 = PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(10),
                xtream_cluster: XtreamCluster::Live,
                source_ordinal: 10,
                ..Default::default()
            },
        };
        playlists.insert_playlist_items(&target, vec![pli10]).await;
        {
            let data = playlists.data.read().await;
            let xtream = data.get(&target.name).unwrap().xtream.as_ref().unwrap();
            assert_eq!(xtream.live_order, vec![(5, 20), (10, 10), (30, 30)]);
        }

        // 5. Add new key 40 via insert_playlist_items: order must binary-search insert 40
        let pli40 = PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(40),
                xtream_cluster: XtreamCluster::Live,
                source_ordinal: 40,
                ..Default::default()
            },
        };
        playlists.insert_playlist_items(&target, vec![pli40]).await;
        {
            let data = playlists.data.read().await;
            let xtream = data.get(&target.name).unwrap().xtream.as_ref().unwrap();
            assert_eq!(xtream.live_order, vec![(5, 20), (10, 10), (30, 30), (40, 40)]);
        }
    }

    #[tokio::test]
    async fn series_episodes_do_not_enter_series_order() {
        let playlists = PlaylistStorageState::new();
        let target = sample_target(true);

        playlists.cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::default())).await;

        // SeriesInfo enters the order
        let series_item = crate::test_config::xtream_series_item(100, None);
        playlists.update_playlist_items(&target, &[&series_item]).await;

        // Series episode does not enter the order
        let mut episode_item = sample_xtream_item(200, XtreamCluster::Series);
        episode_item.item_type = PlaylistItemType::Series;
        playlists.update_playlist_items(&target, &[&episode_item]).await;

        let data = playlists.data.read().await;
        let xtream = data.get(&target.name).unwrap().xtream.as_ref().unwrap();
        assert_eq!(xtream.series_order, vec![(0, 100)]);
        assert!(xtream.series.query(&200).is_some());
    }

    #[tokio::test]
    async fn update_playlist_items_strips_series_details_in_cache() {
        let playlists = PlaylistStorageState::new();
        let target = sample_target(true);

        playlists.cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::default())).await;

        let details = SeriesStreamDetailProperties { year: Some(2024), seasons: None, episodes: None };
        let series_item = crate::test_config::xtream_series_item(77, Some(details));
        assert!(series_item.has_details());

        // Update with series item carrying details
        playlists.update_playlist_items(&target, &[&series_item]).await;

        let data = playlists.data.read().await;
        let xtream = data.get(&target.name).unwrap().xtream.as_ref().unwrap();
        let cached = xtream.series.query(&77).expect("series item present in cache");
        assert!(!cached.has_details(), "cached series item must not have details");
        match &cached.additional_properties {
            Some(StreamProperties::Series(props)) => assert_eq!(props.details, None),
            other => panic!("unexpected properties: {other:?}"),
        }
    }
}
