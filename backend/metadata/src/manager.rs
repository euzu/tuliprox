use crate::ctx::BoundMetadataUpdateCtx;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::Mutex as ParkingMutex;
use std::sync::{
    atomic::{AtomicBool, AtomicI64, AtomicU64},
    Arc,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const METADATA_RETRY_STATE_FILE: &str = "metadata_retry_state.db";

const TASK_ERR_NO_CONNECTION: &str = "No connection available";

const TASK_ERR_PREEMPTED: &str = "Task preempted";

const TASK_ERR_UPDATE_IN_PROGRESS: &str = "Playlist update in progress";

// Per-task execution timeout.  ffprobe defaults to 60 s; allow extra headroom for network
// fetches, TMDB lookups, and B+Tree writes before declaring the task stuck.
const PROBE_TASK_TIMEOUT_SECS: u64 = 30;

const BLOCKING_DB_MIN_CONCURRENCY: usize = 4;

const BLOCKING_DB_MAX_CONCURRENCY: usize = 32;

const RETRY_STATE_PRUNE_INTERVAL_SECS: i64 = 300;

const RETRY_STATE_MIN_TTL_SECS: i64 = 86_400;

const RUNTIME_SETTINGS_REFRESH_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TaskKey {
    Vod(u32),
    VodStr(Arc<str>),
    Series(u32),
    SeriesStr(Arc<str>),
    Live(u32),
    LiveStr(Arc<str>),
    Stream { scope: Arc<str>, id: Arc<str> },
}

/// Manager for background metadata resolution tasks.
///
/// Architecture: Per-Input Worker Pattern
/// - Each input gets its own dedicated worker (tokio task)
/// - Tasks for the SAME input are processed sequentially with rate limiting (defined per task)
/// - Tasks for DIFFERENT inputs run in parallel
/// - Workers are spawned on-demand when first task arrives for an input
/// - Workers terminate after an idle timeout and are respawned on demand
pub struct MetadataUpdateManager {
    /// Per-input worker senders. Worker is spawned when entry is created.
    workers: DashMap<Arc<str>, InputWorkerContext>,
    /// Synchronizes cancellation token rotation with worker context creation.
    worker_lifecycle_lock: ParkingMutex<()>,
    /// Terminal shutdown flag; once set, token rotation must not reactivate workers.
    is_shutdown_flag: AtomicBool,
    /// Global application state (weak reference to avoid cycles)
    ctx: tokio::sync::Mutex<Option<BoundMetadataUpdateCtx>>,
    /// Global gate:
    /// - Foreground playlist updates hold WRITE lock.
    /// - Background metadata/probe tasks hold READ lock per task.
    ///   This guarantees that no background task runs while an update is active.
    update_pause_gate: Arc<RwLock<()>>,
    /// Global cancellation token for shutdown
    cancel_token: ArcSwap<CancellationToken>,
    /// Monotonic worker generation id used to avoid removing a newly spawned worker context.
    next_worker_id: AtomicU64,
    /// Producer-visible view of active resolve cooldowns so repeated playlist refreshes
    /// can skip creating tasks that are still suppressed anyway.
    resolve_enqueue_suppressions: Arc<DashMap<ScopedTaskKey, i64>>,
    /// Producer-visible TMDB/date suppression keyed by the last seen series `last_modified`.
    tmdb_source_markers: Arc<DashMap<ScopedTaskKey, u64>>,
    /// Tracks inputs for which producer-side retry suppression state was already loaded from disk.
    enqueue_state_loaded_inputs: Arc<DashMap<Arc<str>, ()>>,
    /// Backoff for failed producer-side retry-state loads to avoid repeated blocking disk reads.
    enqueue_state_load_retry_at_ts: Arc<DashMap<Arc<str>, i64>>,
    /// Periodic prune marker for stale producer-side resolve suppression entries.
    last_resolve_enqueue_suppression_prune_at_ts: AtomicI64,
}

/// Generate `collect_{vod,series,live}_virtual_updates` from a single body.
/// The three methods differ only in the `BatchResultCollector` field they
/// walk (`vod` / `series` / `live`), the per-kind stream properties type,
/// and the `PlaylistItemType` discriminator used to build provider UUIDs.
macro_rules! collect_virtual_updates {
    ($fn_name:ident, $field:ident, $props_ty:ty, $item_type:expr) => {
        pub(super) fn $fn_name<'a>(
            mapping: &TargetIdMapping,
            input_name: &str,
            batch: &'a BatchResultCollector,
            provider_virtual_ids: &mut HashMap<u32, Vec<VirtualId>>,
            uuid_virtual_ids: &mut HashMap<UUIDType, Option<VirtualId>>,
        ) -> HashMap<VirtualId, &'a $props_ty> {
            let mut virtual_updates: HashMap<VirtualId, &'a $props_ty> = HashMap::new();
            if batch.$field.is_empty() {
                return virtual_updates;
            }

            for (pid, props) in &batch.$field {
                match pid {
                    ProviderIdType::Id(provider_id) => {
                        let virtual_ids = provider_virtual_ids
                            .entry(*provider_id)
                            .or_insert_with(|| mapping.find_virtual_ids(*provider_id));
                        for virtual_id in virtual_ids {
                            virtual_updates.insert(*virtual_id, props);
                        }
                    }
                    ProviderIdType::Text(provider_id_text) => {
                        let uuid = generate_provider_playlist_uuid(input_name, provider_id_text, $item_type);
                        if let Some(virtual_id) = Self::get_cached_uuid_virtual_id(mapping, uuid_virtual_ids, uuid) {
                            virtual_updates.insert(virtual_id, props);
                        }
                    }
                }
            }

            virtual_updates
        }
    };
}

/// Generate `apply_{vod,series,live}_cascade_updates` from a single body.
/// The three methods differ only in the per-kind stream properties type,
/// the `XtreamCluster` enum variant, the `StreamProperties` enum variant
/// used to wrap the boxed property value, and the human-readable label
/// used in error and log messages.
macro_rules! apply_cascade_updates {
    ($fn_name:ident, $props_ty:ty, $cluster:expr, $variant:ident, $log_label:literal) => {
        pub(super) async fn $fn_name<E: EventSink + Clone + 'static>(
            ctx: &MetadataUpdateCtx<E>,
            target: &tuliprox_core::model::ConfigTarget,
            storage_path: &std::path::Path,
            virtual_updates: HashMap<VirtualId, &$props_ty>,
        ) {
            if virtual_updates.is_empty() {
                return;
            }

            let target_name = target.name.as_str();
            let xtream_path = xtream_get_file_path(storage_path, $cluster);
            let updates_input: Vec<(u32, $props_ty)> =
                virtual_updates.into_iter().map(|(vid, props)| (vid.get(), props.clone())).collect();

            let updates = {
                // Scope read lock to read-only query phase so write phase can acquire lock.
                let _file_lock = ctx.app_config.file_locks.read_lock(&xtream_path).await;
                let xtream_path_clone = xtream_path.clone();
                match spawn_blocking_limited(move || -> Result<Vec<XtreamPlaylistItem>, String> {
                    let mut updates = Vec::with_capacity(updates_input.len());
                    let mut query =
                        BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path_clone).map_err(|err| {
                            format!("failed to open {} query at {}: {err}", $log_label, xtream_path_clone.display())
                        })?;
                    for (virtual_id, props) in updates_input {
                        if let Some(mut item) = query.query_zero_copy(&virtual_id).map_err(|err| {
                            format!("failed to query {} {} item {virtual_id}: {err}", $log_label, stringify!($variant))
                        })? {
                            item.additional_properties =
                                Some(shared::model::StreamProperties::$variant(Box::new(props)));
                            updates.push(item);
                        }
                    }
                    Ok(updates)
                })
                .await
                {
                    Ok(Ok(updates)) => updates,
                    Ok(Err(err)) => {
                        error!("Failed to read {} updates from disk for {target_name}: {err}", $log_label);
                        Vec::new()
                    }
                    Err(err) => {
                        error!("Failed to read {} updates from disk for {target_name}: {err}", $log_label);
                        Vec::new()
                    }
                }
            };

            if updates.is_empty() {
                return;
            }

            if let Err(e) = write_playlist_batch_item_upsert(&ctx.app_config, target_name, $cluster, &updates).await {
                error!("Failed to cascade {} updates to target {target_name}: {e}", $log_label);
                return;
            }

            if target.use_memory_cache {
                Self::update_memory_cache(ctx, target_name, $cluster, updates).await;
            }
        }
    };
}

#[cfg(test)]
mod tests;

mod publication;
mod retry;
mod retry_store;
mod settings;
mod task_processing;
mod task_queue;
mod worker;

#[cfg(test)]
use self::task_queue::SubmitTaskResult;
use self::{
    publication::DbHandle,
    retry::{RetryDomain, RetryState, TaskRetryState},
    retry_store::{load_metadata_retry_states_from_disk, persist_metadata_retry_state_to_disk},
    settings::{spawn_blocking_limited, MetadataUpdateRuntimeSettings},
    task_queue::{PendingTask, ScopedTaskKey},
    worker::{InputWorker, InputWorkerContext},
};
