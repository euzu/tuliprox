//! Cached adult EPG-id blocklist for Hide Adult XMLTV filtering.
//!
//! Goal: keep `/xmltv.php` cheap when `hide_adult` is on.
//! - Blocklist (adult ids only) is much smaller than an allowlist of every visible channel.
//! - Shared across all hide-adult users of the same target.
//! - Invalidated by playlist DB mtime/size fingerprint (rebuild after target update).
//! - Prefer in-memory playlist cache when present; otherwise one disk scan, then reuse.

use crate::{
    api::model::AppState,
    model::{Config, ConfigTarget, ProxyUserCredentials},
    repository::{
        get_target_storage_path, m3u_get_file_path_for_db, xtream_get_file_path, xtream_get_storage_path,
        BPlusTreeQuery,
    },
    utils::fold_epg_id_arc,
};
use dashmap::DashMap;
use log::debug;
use shared::model::{M3uPlaylistItem, StreamProperties, XtreamCluster, XtreamPlaylistItem};
use std::{
    collections::HashSet,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use tokio::task;

#[derive(Clone)]
struct AdultEpgIdCacheEntry {
    fingerprint: u64,
    ids: Arc<HashSet<Arc<str>>>,
}

static ADULT_EPG_ID_CACHE: LazyLock<DashMap<u16, AdultEpgIdCacheEntry>> = LazyLock::new(DashMap::new);

#[inline]
fn playlist_item_is_adult(group: &str, props: Option<&StreamProperties>) -> bool {
    ProxyUserCredentials::is_adult_group(group) || ProxyUserCredentials::stream_props_marked_adult(props)
}

#[inline]
fn insert_adult_epg_id(epg_channel_id: Option<&Arc<str>>, into: &mut HashSet<Arc<str>>) {
    let Some(id) = epg_channel_id.filter(|id| !id.is_empty()) else {
        return;
    };
    into.insert(fold_epg_id_arc(id));
}

fn collect_from_m3u_item(item: &M3uPlaylistItem, into: &mut HashSet<Arc<str>>) {
    if !item.item_type.is_live() {
        return;
    }
    if !playlist_item_is_adult(item.group.as_ref(), item.additional_properties.as_ref()) {
        return;
    }
    insert_adult_epg_id(item.epg_channel_id.as_ref(), into);
}

fn collect_from_xtream_item(item: &XtreamPlaylistItem, into: &mut HashSet<Arc<str>>) {
    if item.xtream_cluster != XtreamCluster::Live {
        return;
    }
    if !playlist_item_is_adult(item.group.as_ref(), item.additional_properties.as_ref()) {
        return;
    }
    insert_adult_epg_id(item.epg_channel_id.as_ref(), into);
}

fn path_fingerprint_component(path: &Path, hasher: &mut impl Hasher) {
    path.hash(hasher);
    match fs::metadata(path) {
        Ok(meta) => {
            meta.len().hash(hasher);
            if let Ok(modified) = meta.modified() {
                if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                    duration.as_secs().hash(hasher);
                    duration.subsec_nanos().hash(hasher);
                }
            }
        }
        Err(_) => {
            0u8.hash(hasher);
        }
    }
}

fn playlist_db_paths(config: &Config, target: &ConfigTarget) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(2);
    if let Some(target_path) = get_target_storage_path(config, target.name.as_str()) {
        let m3u_path = m3u_get_file_path_for_db(&target_path);
        if m3u_path.is_file() {
            paths.push(m3u_path);
        }
    }
    if let Some(xtream_path) = xtream_get_storage_path(config, target.name.as_str()) {
        let live_path = xtream_get_file_path(&xtream_path, XtreamCluster::Live);
        if live_path.is_file() {
            paths.push(live_path);
        }
    }
    paths
}

fn playlist_fingerprint(config: &Config, target: &ConfigTarget) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target.id.hash(&mut hasher);
    for path in playlist_db_paths(config, target) {
        path_fingerprint_component(&path, &mut hasher);
    }
    hasher.finish()
}

fn scan_adult_epg_ids_from_disk(paths: Vec<PathBuf>) -> HashSet<Arc<str>> {
    let mut ids = HashSet::new();
    for path in paths {
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if file_name.eq_ignore_ascii_case("live.db") {
            let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&path) else {
                continue;
            };
            for entry in query.iter() {
                let Ok((_, item)) = entry else {
                    continue;
                };
                collect_from_xtream_item(&item, &mut ids);
            }
            continue;
        }
        let Ok(mut query) = BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&path) else {
            continue;
        };
        for entry in query.iter() {
            let Ok((_, item)) = entry else {
                continue;
            };
            collect_from_m3u_item(&item, &mut ids);
        }
    }
    ids
}

async fn collect_adult_epg_ids_from_memory(app_state: &AppState, target_name: &str) -> Option<HashSet<Arc<str>>> {
    // Try to avoid disk when the target playlist is already warm in RAM.
    let guard = app_state.playlists.data.read().await;
    let storage = guard.get(target_name)?;
    if storage.m3u.is_none() && storage.xtream.is_none() {
        return None;
    }
    let mut ids = HashSet::new();
    if let Some(m3u) = storage.m3u.as_ref() {
        for (_, item) in m3u.iter() {
            collect_from_m3u_item(item, &mut ids);
        }
    }
    if let Some(xtream) = storage.xtream.as_ref() {
        for (_, item) in xtream.live.iter() {
            collect_from_xtream_item(item, &mut ids);
        }
    }
    Some(ids)
}

/// Folded EPG ids that belong to adult live playlist items for this target.
///
/// Returns a shared cached set. Empty set means "nothing to hide by id".
pub async fn adult_epg_id_blocklist(app_state: &AppState, target: &ConfigTarget) -> Arc<HashSet<Arc<str>>> {
    let config = app_state.app_config.config.load();
    let fingerprint = playlist_fingerprint(&config, target);

    if let Some(entry) = ADULT_EPG_ID_CACHE.get(&target.id) {
        if entry.fingerprint == fingerprint {
            return Arc::clone(&entry.ids);
        }
    }

    let ids = if let Some(from_memory) = collect_adult_epg_ids_from_memory(app_state, target.name.as_str()).await {
        from_memory
    } else {
        let paths = playlist_db_paths(&config, target);
        match task::spawn_blocking(move || scan_adult_epg_ids_from_disk(paths)).await {
            Ok(ids) => ids,
            Err(err) => {
                debug!("adult EPG id scan join failed for target {}: {err}", target.name);
                HashSet::new()
            }
        }
    };

    debug!(
        "adult EPG blocklist target={} ids={} fingerprint={fingerprint}",
        target.name,
        ids.len()
    );
    let ids = Arc::new(ids);
    ADULT_EPG_ID_CACHE.insert(
        target.id,
        AdultEpgIdCacheEntry {
            fingerprint,
            ids: Arc::clone(&ids),
        },
    );
    ids
}

#[inline]
pub fn epg_channel_hidden_by_adult_blocklist(blocklist: &HashSet<Arc<str>>, channel_id: &Arc<str>) -> bool {
    if blocklist.is_empty() {
        return false;
    }
    blocklist.contains(&fold_epg_id_arc(channel_id))
}

#[cfg(test)]
pub(crate) fn clear_adult_epg_id_cache_for_tests() {
    ADULT_EPG_ID_CACHE.clear();
}

#[cfg(test)]
mod tests {
    use super::{
        collect_from_m3u_item, collect_from_xtream_item, epg_channel_hidden_by_adult_blocklist,
        insert_adult_epg_id, playlist_item_is_adult,
    };
    use shared::model::{
        LiveStreamProperties, M3uPlaylistItem, PlaylistItemType, StreamProperties, XtreamCluster,
        XtreamPlaylistItem,
    };
    use shared::utils::Internable;
    use std::collections::HashSet;

    fn blank_m3u_item() -> M3uPlaylistItem {
        M3uPlaylistItem {
            virtual_id: 0,
            provider_id: "".intern(),
            input_stream_id: "".intern(),
            name: "".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "".intern(),
            epg_channel_id: None,
            input_name: "".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: None,
            upstream_user_agent: None,
        }
    }


    fn blank_xtream_item() -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: 0,
            provider_id: 0,
            name: "".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: None,
            item_type: PlaylistItemType::Live,
            category_id: 0,
            input_name: "".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "".intern(),
            upstream_user_agent: None,
        }
    }


    #[test]
    fn adult_group_live_item_contributes_folded_epg_id() {
        let mut ids = HashSet::new();
        let item = M3uPlaylistItem {
            group: "18+ (Adult)".intern(),
            item_type: PlaylistItemType::Live,
            epg_channel_id: Some("Adult.Channel".intern()),
            ..blank_m3u_item()
        };
        collect_from_m3u_item(&item, &mut ids);
        assert!(ids.contains(&"adult.channel".intern()));
    }

    #[test]
    fn non_adult_and_vod_items_are_ignored() {
        let mut ids = HashSet::new();
        collect_from_m3u_item(
            &M3uPlaylistItem {
                group: "News".intern(),
                item_type: PlaylistItemType::Live,
                epg_channel_id: Some("news.1".intern()),
                ..blank_m3u_item()
            },
            &mut ids,
        );
        collect_from_m3u_item(
            &M3uPlaylistItem {
                group: "XXX".intern(),
                item_type: PlaylistItemType::Video,
                epg_channel_id: Some("vod.adult".intern()),
                ..blank_m3u_item()
            },
            &mut ids,
        );
        assert!(ids.is_empty());
    }

    #[test]
    fn xtream_is_adult_flag_marks_epg_id() {
        let mut ids = HashSet::new();
        collect_from_xtream_item(
            &XtreamPlaylistItem {
                group: "Movies".intern(),
                xtream_cluster: XtreamCluster::Live,
                epg_channel_id: Some("Flagged.Live".intern()),
                additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                    is_adult: 1,
                    ..LiveStreamProperties::default()
                }))),
                ..blank_xtream_item()
            },
            &mut ids,
        );
        assert!(playlist_item_is_adult(
            "Movies",
            Some(&StreamProperties::Live(Box::new(LiveStreamProperties {
                is_adult: 1,
                ..LiveStreamProperties::default()
            })))
        ));
        assert!(ids.contains(&"flagged.live".intern()));
    }

    #[test]
    fn blocklist_lookup_is_case_insensitive() {
        let mut ids = HashSet::new();
        insert_adult_epg_id(Some(&"Mix.Case".intern()), &mut ids);
        assert!(epg_channel_hidden_by_adult_blocklist(&ids, &"mix.case".intern()));
        assert!(epg_channel_hidden_by_adult_blocklist(&ids, &"Mix.Case".intern()));
        assert!(!epg_channel_hidden_by_adult_blocklist(&ids, &"other".intern()));
        assert!(!epg_channel_hidden_by_adult_blocklist(&HashSet::new(), &"mix.case".intern()));
    }
}
