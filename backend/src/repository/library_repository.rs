use crate::model::AppConfig;
use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery};
use shared::error::{TuliproxError, notify_err_res};
use shared::model::{PlaylistGroup, PlaylistItem, UUIDType, XtreamPlaylistItem};
use std::path::Path;
use std::sync::Arc;
use indexmap::IndexMap;

pub async fn persist_input_library_playlist(app_config: &Arc<AppConfig>, library_path: &Path, playlist: &[PlaylistGroup]) -> Result<(), TuliproxError> {
    if playlist.is_empty() {
        return Ok(());
    }
    let _file_lock = app_config.file_locks.write_lock(library_path).await;
    let mut tree = BPlusTree::new();
    for pg in playlist {
        for item in &pg.channels {
            let xtream = XtreamPlaylistItem::from(item);
            tree.insert(item.header.uuid, xtream);
        }
    }
    match tree.store(library_path) {
        Ok(_) => Ok(()),
        Err(err) => notify_err_res!("failed to write local library playlist: {} - {err}", library_path.display())
    }
}


pub async fn load_input_local_library_playlist(app_config: &Arc<AppConfig>, lib_path: &Path) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    let mut groups: IndexMap<Arc<str>, PlaylistGroup> = IndexMap::new();

    if tokio::fs::try_exists(lib_path).await.unwrap_or(false) {
        // Load Items
        let _file_lock = app_config.file_locks.read_lock(lib_path).await;
        if let Ok(mut query) = BPlusTreeQuery::<UUIDType, XtreamPlaylistItem>::try_new(lib_path) {
            let mut group_cnt = 0;
            for (_, ref item) in query.iter() {
                let cat_id = item.group.clone();
                groups.entry(cat_id)
                    .or_insert_with(|| {
                        group_cnt += 1;
                        PlaylistGroup {
                            id: group_cnt,
                            title: item.group.clone(),
                            channels: Vec::new(),
                            xtream_cluster: item.xtream_cluster,
                        }
                    })
                    .channels.push(PlaylistItem::from(item));
            }
        }
    }

    Ok(groups.into_values().collect())
}
