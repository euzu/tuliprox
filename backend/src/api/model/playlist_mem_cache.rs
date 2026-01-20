use std::collections::HashMap;
use tokio::sync::RwLock;
use shared::model::{M3uPlaylistItem, PlaylistItem, XtreamCluster, XtreamPlaylistItem};
use crate::model::ConfigTarget;
use crate::repository::{BPlusTree, VirtualIdRecord};

pub struct PlaylistXtreamStorage {
    pub id_mapping: BPlusTree<u32, VirtualIdRecord>,
    pub live: BPlusTree<u32, XtreamPlaylistItem>,
    pub vod: BPlusTree<u32, XtreamPlaylistItem>,
    pub series: BPlusTree<u32, XtreamPlaylistItem>,
}

pub type PlaylistM3uStorage = BPlusTree<u32, M3uPlaylistItem>;

pub enum PlaylistStorage {
    M3uPlaylist(Box<PlaylistM3uStorage>),
    XtreamPlaylist(Box<PlaylistXtreamStorage>),
}

pub struct TargetPlaylistStorage {
    pub xtream: Option<PlaylistXtreamStorage>,
    pub m3u: Option<PlaylistM3uStorage>,
}

pub type TargetPlaylistStorageMap = HashMap<String, TargetPlaylistStorage>;

pub struct PlaylistStorageState {
    pub data: RwLock<TargetPlaylistStorageMap>,
}

impl PlaylistStorageState {

    pub(crate) fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
        }
    }

    pub async fn update_target_id_mapping(&self, target: &ConfigTarget, mapping: Vec<VirtualIdRecord>) {
        if target.use_memory_cache {
            if let Some(storage) = self.data.write().await.get_mut(&target.name) {
                if let Some(xtream) = storage.xtream.as_mut() {
                    for record in mapping {
                        xtream.id_mapping.insert(record.virtual_id, record);
                    }
                }
            }
        }
    }

    pub async fn update_playlist_items(&self, target: &ConfigTarget, pli_list: Vec<&XtreamPlaylistItem>) {
        if target.use_memory_cache {
            if let Some(storage) = self.data.write().await.get_mut(&target.name) {
                if let Some(xtream) = storage.xtream.as_mut() {
                    for pli in pli_list {
                        match pli.xtream_cluster {
                            XtreamCluster::Live => &mut xtream.live,
                            XtreamCluster::Video => &mut xtream.vod,
                            XtreamCluster::Series =>  &mut xtream.series,
                        }.insert(pli.virtual_id, pli.clone());
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
                        match pli.header.xtream_cluster {
                            XtreamCluster::Live => &mut xtream.live,
                            XtreamCluster::Video => &mut xtream.vod,
                            XtreamCluster::Series =>  &mut xtream.series,
                        }.insert(pli.header.virtual_id, XtreamPlaylistItem::from(&pli));
                    }
                }
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
                        });
                    }
                }
            }
        }
    }
}
