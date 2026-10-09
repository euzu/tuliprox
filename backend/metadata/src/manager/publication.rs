use super::{spawn_blocking_limited, InputWorker, ScopedTaskKey, TaskKey};
use crate::ctx::MetadataUpdateCtx;
use log::error;
use parking_lot::Mutex as ParkingMutex;
use shared::model::{
    EventSink, LiveStreamProperties, SeriesStreamProperties, UUIDType, VideoStreamProperties, VirtualId, XtreamCluster,
    XtreamPlaylistItem,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tuliprox_core::{
    model::{BatchResultCollector, ProviderIdType, UpdateTask},
    utils::FileReadGuard,
};
use tuliprox_repository::{
    get_input_storage_path, get_target_id_mapping_file, persist_input_live_info_batch, persist_input_series_info_batch,
    persist_input_vod_info_batch, xtream_get_file_path, BPlusTreeQuery, TargetIdMapping,
};

pub(super) struct DbHandle {
    pub(super) _guard: FileReadGuard,
    pub(super) query: Arc<ParkingMutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>,
}

impl InputWorker {
    pub(super) fn set_tmdb_source_marker(&self, current_key: &TaskKey, source_last_modified: u64) {
        let scoped_key = ScopedTaskKey::new(self.input_name.clone(), current_key.clone());
        self.tmdb_source_markers.insert(scoped_key, source_last_modified);
    }

    pub(super) fn clear_tmdb_source_marker(&self, current_key: &TaskKey) {
        let scoped_key = ScopedTaskKey::new(self.input_name.clone(), current_key.clone());
        self.tmdb_source_markers.remove(&scoped_key);
    }

    #[inline]
    pub(super) fn should_trigger_playlist_update_for_task(task: &UpdateTask, task_changed: bool) -> bool {
        task_changed && !Self::is_probe_task(task) && !Self::is_probe_only_resolve_task(task)
    }

    pub(super) async fn flush_batch_static<E: EventSink + Clone + 'static>(
        input_name: &str,
        bound_ctx: Option<&MetadataUpdateCtx<E>>,
        batch_buffer: &mut BatchResultCollector,
    ) {
        if batch_buffer.is_empty() {
            return;
        }

        let Some(ctx) = bound_ctx else { return };
        let app_config = &ctx.app_config;
        let cfg = app_config.config.load();
        let vod_updates = batch_buffer.take_vod_updates();
        let series_updates = batch_buffer.take_series_updates();
        let live_updates = batch_buffer.take_live_updates();

        if vod_updates.is_empty() && series_updates.is_empty() && live_updates.is_empty() {
            return;
        }

        if let Ok(storage_path) = get_input_storage_path(input_name, &cfg.storage_dir).await {
            if !vod_updates.is_empty() {
                let mut updates: Vec<(u32, VideoStreamProperties)> = Vec::with_capacity(vod_updates.len());
                for (id, props) in &vod_updates {
                    if let ProviderIdType::Id(vid) = id {
                        updates.push((*vid, props.clone()));
                    }
                }

                if !updates.is_empty() {
                    if let Err(e) = persist_input_vod_info_batch(
                        app_config,
                        &storage_path,
                        XtreamCluster::Video,
                        input_name,
                        updates,
                    )
                    .await
                    {
                        error!("Failed to flush VOD batch for input {input_name}: {e}");
                    }
                }
            }

            if !series_updates.is_empty() {
                let mut updates: Vec<(u32, SeriesStreamProperties)> = Vec::with_capacity(series_updates.len());
                for (id, props) in &series_updates {
                    if let ProviderIdType::Id(vid) = id {
                        updates.push((*vid, props.clone()));
                    }
                }

                if !updates.is_empty() {
                    if let Err(e) = persist_input_series_info_batch(
                        app_config,
                        &storage_path,
                        XtreamCluster::Series,
                        input_name,
                        updates,
                    )
                    .await
                    {
                        error!("Failed to flush Series batch for input {input_name}: {e}");
                    }
                }
            }

            if !live_updates.is_empty() {
                let mut updates: Vec<(u32, LiveStreamProperties)> = Vec::with_capacity(live_updates.len());
                for (id, props) in &live_updates {
                    if let ProviderIdType::Id(vid) = id {
                        updates.push((*vid, props.clone()));
                    }
                }

                if !updates.is_empty() {
                    if let Err(e) = persist_input_live_info_batch(
                        app_config,
                        &storage_path,
                        XtreamCluster::Live,
                        input_name,
                        updates,
                    )
                    .await
                    {
                        error!("Failed to flush Live batch for input {input_name}: {e}");
                    }
                }
            }
        }

        let cascade_batch = BatchResultCollector { vod: vod_updates, series: series_updates, live: live_updates };

        Self::cascade_updates(ctx, &app_config.config.load(), input_name, &cascade_batch).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn cascade_updates<E: EventSink + Clone + 'static>(
        ctx: &MetadataUpdateCtx<E>,
        config: &tuliprox_core::model::Config,
        input_name: &str,
        batch: &BatchResultCollector,
    ) {
        if batch.is_empty() {
            return;
        }

        // Find targets affected by this input.
        let targets = {
            let sources = ctx.app_config.sources.load();
            let mut affected_targets = Vec::new();

            for source in &sources.sources {
                if source.inputs.iter().any(|i_name| i_name.as_ref() == input_name) {
                    for t_def in &source.targets {
                        affected_targets.push(t_def.clone());
                    }
                }
            }
            affected_targets
        };

        if targets.is_empty() {
            return;
        }

        for target in targets {
            let target_name = &target.name;
            let Some(target_path) = tuliprox_repository::get_target_storage_path(config, target_name) else {
                continue;
            };
            let Some(storage_path) = tuliprox_repository::xtream_get_storage_path(config, target_name) else {
                continue;
            };
            let mapping_file = get_target_id_mapping_file(&target_path);

            let mapping = {
                // Scope read lock strictly to mapping load.
                let _file_lock = ctx.app_config.file_locks.read_lock(&mapping_file).await;
                let mapping_file_clone = mapping_file.clone();
                match spawn_blocking_limited(move || TargetIdMapping::new(&mapping_file_clone, false)).await {
                    Ok(Ok(mapping)) => mapping,
                    Ok(Err(e)) => {
                        error!("Failed to open ID mapping for target {target_name}: {e}");
                        continue;
                    }
                    Err(err) => {
                        error!("Failed to open ID mapping for target {target_name}: {err}");
                        continue;
                    }
                }
            };

            let mut provider_virtual_ids: HashMap<u32, Vec<VirtualId>> = HashMap::new();
            let mut uuid_virtual_ids: HashMap<UUIDType, Option<VirtualId>> = HashMap::new();

            let vod_virtual_updates = Self::collect_vod_virtual_updates(
                &mapping,
                input_name,
                batch,
                &mut provider_virtual_ids,
                &mut uuid_virtual_ids,
            );
            Self::apply_vod_cascade_updates(ctx, &target, &storage_path, vod_virtual_updates).await;

            let series_virtual_updates = Self::collect_series_virtual_updates(
                &mapping,
                input_name,
                batch,
                &mut provider_virtual_ids,
                &mut uuid_virtual_ids,
            );
            Self::apply_series_cascade_updates(ctx, &target, &storage_path, series_virtual_updates).await;

            let live_virtual_updates = Self::collect_live_virtual_updates(
                &mapping,
                input_name,
                batch,
                &mut provider_virtual_ids,
                &mut uuid_virtual_ids,
            );
            Self::apply_live_cascade_updates(ctx, &target, &storage_path, live_virtual_updates).await;
        }
    }

    pub(super) fn get_cached_uuid_virtual_id(
        mapping: &TargetIdMapping,
        cache: &mut HashMap<UUIDType, Option<VirtualId>>,
        uuid: UUIDType,
    ) -> Option<VirtualId> {
        if let Some(cached) = cache.get(&uuid) {
            return *cached;
        }
        let resolved = mapping.get_virtual_id_by_uuid(&uuid);
        cache.insert(uuid, resolved);
        resolved
    }

    pub(super) async fn update_memory_cache<E: EventSink + Clone + 'static>(
        ctx: &MetadataUpdateCtx<E>,
        target_name: &str,
        cluster: XtreamCluster,
        updates: Vec<XtreamPlaylistItem>,
    ) {
        let mut playlists = ctx.playlists.data.write().await;
        if let Some(playlist) = playlists.get_mut(target_name) {
            if let Some(xtream_storage) = &mut playlist.xtream {
                let storage = match cluster {
                    XtreamCluster::Live => &mut xtream_storage.live,
                    XtreamCluster::Video => &mut xtream_storage.vod,
                    XtreamCluster::Series => &mut xtream_storage.series,
                };
                for item in updates {
                    storage.insert(item.virtual_id.get(), item);
                }
            }
        }
    }
    pub(super) async fn get_or_open_query<E: EventSink + Clone + 'static>(
        input_name: &str,
        ctx: &MetadataUpdateCtx<E>,
        cluster: XtreamCluster,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Option<Arc<ParkingMutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>> {
        if failed_clusters.contains(&cluster) {
            return None;
        }

        if let std::collections::hash_map::Entry::Vacant(entry) = db_handles.entry(cluster) {
            let cfg = ctx.app_config.config.load();
            if let Ok(storage_path) = get_input_storage_path(input_name, &cfg.storage_dir).await {
                let file_path = xtream_get_file_path(&storage_path, cluster);
                if file_path.exists() {
                    let lock = ctx.app_config.file_locks.read_lock(&file_path).await;
                    let file_path = file_path.clone();
                    let query = match spawn_blocking_limited(move || {
                        BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&file_path)
                    })
                    .await
                    {
                        Ok(Ok(query)) => Some(query),
                        Ok(Err(err)) => {
                            error!("Failed to open BPlusTreeQuery for {cluster}: {err}");
                            None
                        }
                        Err(err) => {
                            error!("Failed to open BPlusTreeQuery for {cluster}: {err}");
                            None
                        }
                    };

                    if let Some(query) = query {
                        entry.insert(DbHandle { _guard: lock, query: Arc::new(ParkingMutex::new(query)) });
                    } else {
                        failed_clusters.insert(cluster);
                    }
                } else {
                    // File doesn't exist; do not mark as failure to allow future creation.
                }
            }
        }

        db_handles.get(&cluster).map(|h| Arc::clone(&h.query))
    }
}
