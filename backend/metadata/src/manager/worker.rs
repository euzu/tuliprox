use super::{
    spawn_blocking_limited, DbHandle, MetadataUpdateManager, MetadataUpdateRuntimeSettings, PendingTask, RetryDomain,
    RetryState, ScopedTaskKey, TaskKey, TaskRetryState, RUNTIME_SETTINGS_REFRESH_INTERVAL_SECS,
    TASK_ERR_UPDATE_IN_PROGRESS,
};
use crate::ctx::{BoundMetadataUpdateCtx, MetadataUpdateCtx};
use dashmap::{mapref::entry::Entry, DashMap};
use log::{debug, error, info, warn};
use shared::{
    model::{
        EventMessage, EventSink, LiveStreamProperties, MetadataUpdateFailure, PlaylistItemType, SeriesStreamProperties,
        UUIDType, VideoStreamProperties, VirtualId, XtreamCluster, XtreamPlaylistItem,
    },
    utils::{generate_provider_playlist_uuid, sanitize_sensitive_info},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{BatchResultCollector, ProviderIdType, ResolveReasonSet, UpdateTask};
use tuliprox_repository::{write_playlist_batch_item_upsert, xtream_get_file_path, BPlusTreeQuery, TargetIdMapping};

/// Per-input worker context. Each input has its own worker
/// that processes tasks sequentially with rate limiting.
#[derive(Clone)]
pub(super) struct InputWorkerContext {
    pub(super) worker_id: u64,
    pub(super) sender: mpsc::Sender<TaskKey>,
    pub(super) pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
    pub(super) pending_task_count: Arc<AtomicUsize>,
}

impl MetadataUpdateManager {
    /// Requests graceful shutdown for all metadata workers and delayed requeue tasks.
    /// This operation is idempotent.
    pub fn shutdown(&self) {
        let _guard = self.worker_lifecycle_lock.lock();
        self.is_shutdown_flag.store(true, Ordering::Release);
        self.cancel_token.load_full().cancel();
        self.resolve_enqueue_suppressions.clear();
        self.tmdb_source_markers.clear();
        self.enqueue_state_loaded_inputs.clear();
        self.enqueue_state_load_retry_at_ts.clear();
    }

    /// Returns true once shutdown has been requested.
    pub fn is_shutdown(&self) -> bool { self.is_shutdown_flag.load(Ordering::Acquire) }

    /// Rotates the cancellation token for metadata workers.
    /// Existing workers are cancelled and removed so new tasks start with fresh runtime state.
    pub fn rotate_cancel_token(&self, cancel_token: CancellationToken) {
        let old_token = {
            let _guard = self.worker_lifecycle_lock.lock();
            if self.is_shutdown_flag.load(Ordering::Acquire) {
                self.workers.clear();
                cancel_token.cancel();
                return;
            }

            let old_token = self.cancel_token.swap(Arc::new(cancel_token));
            self.workers.clear();
            self.resolve_enqueue_suppressions.clear();
            self.tmdb_source_markers.clear();
            self.enqueue_state_loaded_inputs.clear();
            self.enqueue_state_load_retry_at_ts.clear();
            old_token
        };
        old_token.cancel();
    }

    /// Acquire exclusive gate for a foreground playlist update.
    /// While this guard is held, background workers wait before starting heavy metadata/probe steps.
    pub async fn acquire_update_pause_guard(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.update_pause_gate.clone().write_owned().await
    }

    pub(super) fn remove_worker_context_if_id(
        workers: &DashMap<Arc<str>, InputWorkerContext>,
        input_name: &Arc<str>,
        worker_id: u64,
    ) {
        if let Entry::Occupied(entry) = workers.entry(input_name.clone()) {
            if entry.get().worker_id == worker_id {
                entry.remove();
            }
        }
    }

    /// Get the number of active workers (for monitoring/debugging)
    pub fn active_worker_count(&self) -> usize { self.workers.len() }
}

pub(super) struct InputWorker {
    pub(super) input_name: Arc<str>,
    pub(super) sender: mpsc::Sender<TaskKey>,
    pub(super) receiver: mpsc::Receiver<TaskKey>,
    pub(super) pending_tasks: Arc<DashMap<TaskKey, PendingTask>>,
    pub(super) pending_task_count: Arc<AtomicUsize>,
    pub(super) bound_ctx: Option<BoundMetadataUpdateCtx>,
    pub(super) update_pause_gate: Arc<RwLock<()>>,
    pub(super) cancel_token: CancellationToken,
    pub(super) batch_buffer: BatchResultCollector,
    pub(super) db_handles: HashMap<XtreamCluster, DbHandle>,
    pub(super) failed_clusters: HashSet<XtreamCluster>,
    pub(super) retry_states: HashMap<TaskKey, TaskRetryState>,
    pub(super) resolve_exhausted: HashMap<TaskKey, i64>,
    pub(super) last_cycle_completed_at_ts: Option<i64>,
    pub(super) metadata_retry_state_path: Option<PathBuf>,
    pub(super) metadata_retry_loaded: bool,
    pub(super) metadata_retry_load_retry_at_ts: Option<i64>,
    pub(super) last_retry_state_prune_at_ts: Option<i64>,
    // Shared with detached delayed requeue tasks spawned in `schedule_requeue_at`.
    // A plain HashMap cannot be moved safely into those `'static` tasks.
    pub(super) scheduled_requeues: Arc<DashMap<TaskKey, i64>>,
    // Cache of tasks that recently completed with no changes (Ok(None)).
    // Prevents repeated resolution of already-resolved items across playlist refreshes.
    // Stores the reason set so that tasks with new/different reasons are not wrongly skipped.
    pub(super) recently_completed_no_change: HashMap<TaskKey, (Instant, ResolveReasonSet)>,
    pub(super) resolve_enqueue_suppressions: Arc<DashMap<ScopedTaskKey, i64>>,
    pub(super) tmdb_source_markers: Arc<DashMap<ScopedTaskKey, u64>>,
    pub(super) dirty_retry_state_keys: HashSet<TaskKey>,
}

impl InputWorker {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn run(mut self) {
        debug!("Metadata worker started for input {}", self.input_name);

        let mut processed_vod_count: usize = 0;
        let mut processed_series_count: usize = 0;
        let mut last_queue_log_at = Instant::now();
        let mut last_progress_log_at = Instant::now();
        let mut queue_cycle_active = false;
        let mut cycle_had_changes = false;
        // Tasks that burned through their retries this cycle. A cycle that
        // exhausts work is the only signal an operator gets that an input has
        // stopped resolving - `InputMetadataUpdatesCompleted` fires only when
        // something actually changed, so a permanently broken input would
        // otherwise emit a start and then nothing at all.
        let mut cycle_failed_tasks: usize = 0;
        let mut cycle_last_error: Option<String> = None;
        let mut consecutive_resolve_tasks = 0_usize;

        let input_name = self.input_name.clone();
        let bound_ctx = self.bound_ctx.clone();

        let mut runtime_settings = self.runtime_settings();
        let mut last_runtime_settings_refresh_at = Instant::now();
        self.ensure_metadata_retry_state_loaded(&input_name, bound_ctx.as_ref(), &runtime_settings).await;

        // Keep one prefetched task to minimize channel waits/lock churn.
        let mut next_task: Option<(TaskKey, UpdateTask, u64)> = None;

        loop {
            if last_runtime_settings_refresh_at.elapsed() >= Duration::from_secs(RUNTIME_SETTINGS_REFRESH_INTERVAL_SECS)
            {
                runtime_settings = self.runtime_settings();
                last_runtime_settings_refresh_at = Instant::now();
            }
            let prefer_probe = consecutive_resolve_tasks >= runtime_settings.probe_fairness_resolve_burst;
            let task_data = if let Some(prefetched) = next_task.take() {
                if prefer_probe && Self::retry_domain_for_task(&prefetched.1) == RetryDomain::Resolve {
                    if let Some(probe_task) = self.take_pending_probe_task_snapshot() {
                        next_task = Some(prefetched);
                        Some(probe_task)
                    } else {
                        Some(prefetched)
                    }
                } else {
                    Some(prefetched)
                }
            } else if prefer_probe {
                if let Some(probe_task) = self.take_pending_probe_task_snapshot() {
                    Some(probe_task)
                } else {
                    self.recv_task_fast_or_wait(&runtime_settings).await
                }
            } else {
                self.recv_task_fast_or_wait(&runtime_settings).await
            };

            let Some((current_key, current_task, current_generation)) = task_data else { break };
            if self.cancel_token.is_cancelled() {
                break;
            }
            if !self.metadata_retry_loaded {
                self.ensure_metadata_retry_state_loaded(&input_name, bound_ctx.as_ref(), &runtime_settings).await;
            }
            let now_ts = chrono::Utc::now().timestamp();
            self.prune_retry_tracking_maps_if_needed(now_ts, &runtime_settings).await;

            if !queue_cycle_active {
                // First entry of a new processing cycle.
                queue_cycle_active = true;
                cycle_had_changes = false;
                cycle_failed_tasks = 0;
                cycle_last_error = None;
                processed_vod_count = 0;
                processed_series_count = 0;
                last_progress_log_at = Instant::now();
                // Emit queue logs promptly for the new cycle.
                last_queue_log_at = Instant::now()
                    .checked_sub(runtime_settings.queue_log_interval + Duration::from_secs(1))
                    .unwrap_or_else(Instant::now);
                if let Some(ctx) = bound_ctx.clone() {
                    ctx.events.emit(EventMessage::InputMetadataUpdatesStarted(input_name.clone()));
                }
                if self.last_cycle_completed_at_ts.is_some_and(|last| {
                    now_ts.saturating_sub(last) >= runtime_settings.resolve_exhaustion_reset_gap_secs
                }) {
                    self.resolve_exhausted.clear();
                }
                debug!("Background metadata update queue has entries for input {input_name}; starting processing");
            }

            let current_retry_domain = Self::retry_domain_for_task(&current_task);
            let delay_secs = current_task.delay();
            let mut schedule_requeue_at_ts: Option<i64> = None;
            let mut remove_current_task = false;
            let mut apply_rate_limit = false;
            let mut metadata_persist_state: Option<Option<TaskRetryState>> = None;
            let mut skip_execution = false;
            let mut task_for_execution = current_task.clone();

            if Self::is_resolve_task(&task_for_execution) && self.resolve_exhausted.contains_key(&current_key) {
                debug!(
                    "[Metadata-Task] Skipping task (resolve exhausted) for input {}: {} (reset window: {}s)",
                    input_name, task_for_execution, runtime_settings.resolve_exhaustion_reset_gap_secs
                );
                self.scheduled_requeues.remove(&current_key);
                remove_current_task = true;
                skip_execution = true;
            }

            // Skip tasks that recently completed with no changes (already resolved in DB).
            // Only skip when the incoming reason set exactly matches the cached reason set.
            if !skip_execution
                && self.should_skip_recent_no_change_task(&current_key, &task_for_execution, &runtime_settings)
            {
                debug!(
                    "[Metadata-Task] Skipping task (recently completed with no changes) for input {input_name}: {task_for_execution}",
                );
                remove_current_task = true;
                skip_execution = true;
            }

            if !skip_execution {
                let mut clear_tmdb_state = false;
                if let Some(state_bundle) = self.retry_states.get(&current_key) {
                    if let Some(tmdb_state) = state_bundle.get(RetryDomain::Tmdb) {
                        let task_has_tmdb_reason = Self::task_has_tmdb_reason(&task_for_execution);
                        let source_unchanged = tmdb_state.source_last_modified.is_some()
                            && tmdb_state.source_last_modified == Self::task_source_last_modified(&task_for_execution);
                        if source_unchanged && task_has_tmdb_reason {
                            if let Some(stripped_task) = Self::strip_tmdb_reasons(&task_for_execution) {
                                debug!(
                                    "[Task] TMDB/date unchanged for input {}: {}, continuing with non-TMDB reasons (last_modified={:?})",
                                    input_name,
                                    task_for_execution,
                                    tmdb_state.source_last_modified
                                );
                                task_for_execution = stripped_task;
                            } else {
                                debug!(
                                    "[Metadata-Task] Skipping task (TMDB/date unresolved and source unchanged) for input {}: {} (last_modified={:?})",
                                    input_name,
                                    task_for_execution,
                                    tmdb_state.source_last_modified
                                );
                                self.scheduled_requeues.remove(&current_key);
                                remove_current_task = true;
                                skip_execution = true;
                            }
                        } else if task_has_tmdb_reason {
                            if let Some(cooldown_until_ts) = tmdb_state.cooldown_until_ts {
                                if now_ts < cooldown_until_ts {
                                    if let Some(stripped_task) = Self::strip_tmdb_reasons(&task_for_execution) {
                                        debug!(
                                            "[Task] TMDB cooldown active for input {}: {}, continuing with non-TMDB reasons (cooldown_until={}, remaining={}s)",
                                            input_name,
                                            task_for_execution,
                                            cooldown_until_ts,
                                            cooldown_until_ts.saturating_sub(now_ts)
                                        );
                                        task_for_execution = stripped_task;
                                    } else {
                                        debug!(
                                            "[Metadata-Task] Skipping task (TMDB-only in cooldown) for input {}: {} (cooldown_until={}, remaining={}s)",
                                            input_name,
                                            task_for_execution,
                                            cooldown_until_ts,
                                            cooldown_until_ts.saturating_sub(now_ts)
                                        );
                                        self.scheduled_requeues.remove(&current_key);
                                        remove_current_task = true;
                                        skip_execution = true;
                                    }
                                } else {
                                    clear_tmdb_state = true;
                                }
                            }
                        }
                    }
                }

                let active_retry_domain = Self::retry_domain_for_task(&task_for_execution);
                let mut clear_active_retry_state = false;

                if !skip_execution {
                    if let Some(state_bundle) = self.retry_states.get(&current_key) {
                        if let Some(active_state) = state_bundle.get(active_retry_domain) {
                            if let Some(cooldown_until_ts) = active_state.cooldown_until_ts {
                                if now_ts < cooldown_until_ts {
                                    let cooldown_label =
                                        if active_retry_domain == RetryDomain::Probe { "probe" } else { "resolve" };
                                    debug!(
                                        "[Metadata-Task] Skipping task ({} cooldown) for input {}: {} (cooldown_until={}, remaining={}s)",
                                        cooldown_label,
                                        input_name,
                                        task_for_execution,
                                        cooldown_until_ts,
                                        cooldown_until_ts.saturating_sub(now_ts)
                                    );
                                    self.scheduled_requeues.remove(&current_key);
                                    remove_current_task = true;
                                    skip_execution = true;
                                } else {
                                    clear_active_retry_state = true;
                                }
                            }

                            if !skip_execution && active_state.next_allowed_at_ts > now_ts {
                                debug!(
                                    "[Task] Deferring task (retry backoff) for input {}: {} (next_allowed_at={}, wait={}s, attempts={})",
                                    input_name,
                                    task_for_execution,
                                    active_state.next_allowed_at_ts,
                                    active_state.next_allowed_at_ts.saturating_sub(now_ts),
                                    active_state.attempts
                                );
                                schedule_requeue_at_ts = Some(active_state.next_allowed_at_ts);
                                skip_execution = true;
                            }
                        }
                    }
                }

                if clear_active_retry_state || clear_tmdb_state {
                    let mut should_remove_retry_entry = false;
                    let mut state_after_clear: Option<TaskRetryState> = None;

                    if let Some(state_bundle) = self.retry_states.get_mut(&current_key) {
                        if clear_active_retry_state {
                            state_bundle.clear_domain(active_retry_domain);
                        }
                        if clear_tmdb_state {
                            state_bundle.clear_domain(RetryDomain::Tmdb);
                        }
                        if state_bundle.is_empty() {
                            should_remove_retry_entry = true;
                        } else {
                            state_bundle.touch(now_ts);
                            state_after_clear = Some(state_bundle.clone());
                        }
                    }

                    if should_remove_retry_entry {
                        self.retry_states.remove(&current_key);
                    }
                    if clear_active_retry_state && active_retry_domain == RetryDomain::Resolve {
                        self.clear_resolve_enqueue_suppression(&current_key);
                    }
                    if clear_tmdb_state {
                        self.clear_tmdb_source_marker(&current_key);
                    }
                    metadata_persist_state = Some(state_after_clear);
                }
            }

            if !skip_execution {
                debug!(
                    "[Task] Executing task for input {}: {} (retry_domain={:?})",
                    input_name,
                    task_for_execution,
                    Self::retry_domain_for_task(&task_for_execution)
                );
                let task_result = {
                    let Some(_pause_guard) = self.wait_for_update_pause_window().await else {
                        break;
                    };
                    Self::process_task_static(
                        &input_name,
                        bound_ctx.as_ref(),
                        &task_for_execution,
                        &mut self.batch_buffer,
                        &mut self.db_handles,
                        &mut self.failed_clusters,
                    )
                    .await
                };

                match task_result {
                    Ok(task_outcome) => {
                        if Self::is_vod_task_key(&current_key) {
                            processed_vod_count += 1;
                        } else if Self::is_series_task_key(&current_key) {
                            processed_series_count += 1;
                        }
                        let trigger_playlist_update = Self::should_trigger_playlist_update_for_task(
                            &task_for_execution,
                            task_outcome.task_changed,
                        );
                        cycle_had_changes |= trigger_playlist_update;
                        debug!(
                            "[Metadata-Task] Task succeeded for input {input_name}: {task_for_execution} (changed={}, trigger_playlist_update={}, tmdb_pending={}, probe_pending={})",
                            task_outcome.task_changed,
                            trigger_playlist_update,
                            task_outcome.tmdb_pending,
                            task_outcome.probe_pending
                        );

                        let active_retry_domain = Self::retry_domain_for_task(&task_for_execution);
                        let task_has_tmdb_reason = Self::task_has_tmdb_reason(&task_for_execution);
                        let task_source_last_modified = Self::task_source_last_modified(&task_for_execution);
                        let mut should_remove_retry_entry = false;
                        let mut clear_tmdb_marker = false;
                        let mut state_after_success: Option<TaskRetryState> = None;

                        if let Some(state_bundle) = self.retry_states.get_mut(&current_key) {
                            state_bundle.clear_domain(active_retry_domain);
                            if task_has_tmdb_reason {
                                if task_outcome.tmdb_pending {
                                    let cooldown_until_ts = now_ts.saturating_add(runtime_settings.tmdb_cooldown_secs);
                                    let tmdb_state = state_bundle.get_mut_or_insert(RetryDomain::Tmdb);
                                    tmdb_state.attempts = 0;
                                    tmdb_state.next_allowed_at_ts = cooldown_until_ts;
                                    tmdb_state.cooldown_until_ts = Some(cooldown_until_ts);
                                    tmdb_state.last_error =
                                        Some("TMDB lookup completed without matching result".to_string());
                                    tmdb_state.source_last_modified = task_source_last_modified;
                                    debug!(
                                        "[Metadata-Task] TMDB resolve produced no match (existing retry state), entering cooldown for input {}: {} (cooldown_until={}, cooldown_duration={}s)",
                                        input_name,
                                        task_for_execution,
                                        cooldown_until_ts,
                                        runtime_settings.tmdb_cooldown_secs
                                    );
                                } else {
                                    state_bundle.clear_domain(RetryDomain::Tmdb);
                                    clear_tmdb_marker = true;
                                }
                            }

                            if state_bundle.is_empty() {
                                should_remove_retry_entry = true;
                            } else {
                                state_bundle.touch(now_ts);
                                state_after_success = Some(state_bundle.clone());
                            }
                        } else if task_has_tmdb_reason && task_outcome.tmdb_pending {
                            let cooldown_until_ts = now_ts.saturating_add(runtime_settings.tmdb_cooldown_secs);
                            let state_bundle = TaskRetryState {
                                resolve: None,
                                probe: None,
                                tmdb: Some(RetryState {
                                    attempts: 0,
                                    next_allowed_at_ts: cooldown_until_ts,
                                    cooldown_until_ts: Some(cooldown_until_ts),
                                    last_error: Some("TMDB lookup completed without matching result".to_string()),
                                    source_last_modified: task_source_last_modified,
                                }),
                                updated_at_ts: now_ts.max(1),
                            };
                            self.retry_states.insert(current_key.clone(), state_bundle.clone());
                            state_after_success = Some(state_bundle);
                            debug!(
                                "[Metadata-Task] TMDB resolve produced no match (new retry state), entering cooldown for input {}: {} (cooldown_until={}, cooldown_duration={}s)",
                                input_name,
                                task_for_execution,
                                cooldown_until_ts,
                                runtime_settings.tmdb_cooldown_secs
                            );
                        } else if task_has_tmdb_reason {
                            self.clear_tmdb_source_marker(&current_key);
                        }

                        if should_remove_retry_entry {
                            self.retry_states.remove(&current_key);
                        }
                        if clear_tmdb_marker {
                            self.clear_tmdb_source_marker(&current_key);
                        }
                        if task_outcome.tmdb_pending {
                            if let Some(source_last_modified) = task_source_last_modified {
                                self.set_tmdb_source_marker(&current_key, source_last_modified);
                            } else {
                                self.clear_tmdb_source_marker(&current_key);
                            }
                        }
                        if should_remove_retry_entry || state_after_success.is_some() {
                            metadata_persist_state = Some(state_after_success);
                        }

                        self.resolve_exhausted.remove(&current_key);
                        if active_retry_domain == RetryDomain::Resolve {
                            self.clear_resolve_enqueue_suppression(&current_key);
                        }
                        self.scheduled_requeues.remove(&current_key);

                        // Cache tasks that completed with no changes to skip redundant re-resolution.
                        if !task_outcome.task_changed
                            && Self::is_resolve_task(&task_for_execution)
                            && !task_outcome.tmdb_pending
                            && !task_outcome.probe_pending
                        {
                            let reasons = Self::task_reason(&task_for_execution);
                            debug!(
                                "[Task] Caching no-change result for input {}: {} (reasons={}, ttl={}s)",
                                input_name, task_for_execution, reasons, runtime_settings.no_change_cache_ttl_secs
                            );
                            self.recently_completed_no_change.insert(current_key.clone(), (Instant::now(), reasons));
                            // Set producer-side enqueue suppression so that future playlist
                            // processing cycles do not re-queue this task during the TTL.
                            let suppressed_until_ts = now_ts.saturating_add(
                                i64::try_from(runtime_settings.no_change_cache_ttl_secs).unwrap_or(i64::MAX),
                            );
                            self.set_resolve_enqueue_suppression(&current_key, suppressed_until_ts);
                        } else {
                            self.recently_completed_no_change.remove(&current_key);
                        }

                        if last_progress_log_at.elapsed() >= runtime_settings.progress_log_interval {
                            // current_key is removed from pending_tasks later in this loop iteration;
                            // subtract it here so "remaining" reflects the post-success queue size.
                            let (mut remaining_vod, mut remaining_series) =
                                Self::queue_resolve_counts(&self.pending_tasks);
                            if Self::is_vod_task_key(&current_key) {
                                remaining_vod = remaining_vod.saturating_sub(1);
                            } else if Self::is_series_task_key(&current_key) {
                                remaining_series = remaining_series.saturating_sub(1);
                            }

                            let total_vod = processed_vod_count.saturating_add(remaining_vod);
                            let total_series = processed_series_count.saturating_add(remaining_series);
                            let resolved_total = processed_vod_count.saturating_add(processed_series_count);
                            let total_resolve = total_vod.saturating_add(total_series);

                            info!("Background metadata update: {resolved_total} / {total_resolve} resolved for input {input_name} (vod: {processed_vod_count}/{total_vod}, series: {processed_series_count}/{total_series})");
                            last_progress_log_at = Instant::now();
                        }

                        remove_current_task = true;
                        apply_rate_limit = true;
                    }
                    Err(e) => {
                        if Self::is_permanent_not_found_error(e.message()) {
                            debug!(
                                "[Task] Task failed with permanent not-found for input {}: {} (error={})",
                                input_name,
                                task_for_execution,
                                e.message()
                            );
                            let retry_domain = Self::retry_domain_for_task(&task_for_execution);
                            self.scheduled_requeues.remove(&current_key);
                            if retry_domain == RetryDomain::Probe {
                                let cooldown_until_ts = now_ts.saturating_add(runtime_settings.probe_cooldown_secs);
                                let state_bundle_after_update = {
                                    let state_bundle = self.retry_states.entry(current_key.clone()).or_default();
                                    let state = state_bundle.get_mut_or_insert(RetryDomain::Probe);
                                    state.attempts = runtime_settings.max_attempts_probe;
                                    state.next_allowed_at_ts = cooldown_until_ts;
                                    state.cooldown_until_ts = Some(cooldown_until_ts);
                                    state.last_error = Some(e.message().to_string());
                                    state_bundle.touch(now_ts);
                                    state_bundle.clone()
                                };
                                metadata_persist_state = Some(Some(state_bundle_after_update));
                                remove_current_task = true;
                                debug!(
                                    "[Metadata-Task] Probe task entering cooldown after permanent not-found for input {}: {} (cooldown_until={}, cooldown_duration={}s)",
                                    input_name,
                                    task_for_execution,
                                    cooldown_until_ts,
                                    runtime_settings.probe_cooldown_secs
                                );
                            } else {
                                let cooldown_until_ts =
                                    now_ts.saturating_add(runtime_settings.resolve_exhaustion_reset_gap_secs);
                                let state_bundle_after_update = {
                                    let state_bundle = self.retry_states.entry(current_key.clone()).or_default();
                                    let state = state_bundle.get_mut_or_insert(RetryDomain::Resolve);
                                    state.attempts = runtime_settings.max_attempts_resolve;
                                    state.next_allowed_at_ts = cooldown_until_ts;
                                    state.cooldown_until_ts = Some(cooldown_until_ts);
                                    state.last_error = Some(e.message().to_string());
                                    state_bundle.touch(now_ts);
                                    state_bundle.clone()
                                };
                                self.set_resolve_enqueue_suppression(&current_key, cooldown_until_ts);
                                self.resolve_exhausted.insert(current_key.clone(), now_ts);
                                metadata_persist_state = Some(Some(state_bundle_after_update));
                                remove_current_task = true;
                                debug!(
                                    "[Metadata-Task] Resolve task entering cooldown after permanent not-found for input {}: {} (cooldown_until={}, cooldown_duration={}s, error={})",
                                    input_name,
                                    task_for_execution,
                                    cooldown_until_ts,
                                    runtime_settings.resolve_exhaustion_reset_gap_secs,
                                    e.message()
                                );
                            }
                        } else if Self::is_transient_worker_error(e.message()) {
                            if e.message() == TASK_ERR_UPDATE_IN_PROGRESS {
                                // Drop cached readers quickly so foreground writer can progress.
                                self.release_db_handles();
                            }

                            let retry_delay_secs =
                                Self::compute_retry_delay_secs(current_task.delay(), &runtime_settings);
                            let retry_delay_i64 = i64::try_from(retry_delay_secs).unwrap_or(i64::MAX);
                            schedule_requeue_at_ts = Some(now_ts.saturating_add(retry_delay_i64));
                            debug!(
                                "[Metadata-Task] Task deferred (transient error) for input {}: {} (retry_in={}s, error={})",
                                input_name,
                                task_for_execution,
                                retry_delay_secs,
                                e.message()
                            );
                        } else {
                            let retry_domain = Self::retry_domain_for_task(&task_for_execution);
                            let max_attempts = if retry_domain == RetryDomain::Probe {
                                runtime_settings.max_attempts_probe
                            } else {
                                runtime_settings.max_attempts_resolve
                            };

                            let (state_after_update, state_bundle_after_update) = {
                                let state_bundle = self.retry_states.entry(current_key.clone()).or_default();
                                let state_after_update = {
                                    let state = state_bundle.get_mut_or_insert(retry_domain);
                                    state.attempts = state.attempts.saturating_add(1);
                                    state.last_error = Some(e.message().to_string());

                                    if state.attempts < max_attempts {
                                        let backoff_secs = if retry_domain == RetryDomain::Probe {
                                            Self::compute_probe_retry_backoff_secs(state.attempts, &runtime_settings)
                                        } else {
                                            Self::compute_resolve_retry_backoff_secs(
                                                task_for_execution.delay(),
                                                state.attempts,
                                                &runtime_settings,
                                            )
                                        };
                                        let backoff_i64 = i64::try_from(backoff_secs).unwrap_or(i64::MAX);
                                        state.next_allowed_at_ts = now_ts.saturating_add(backoff_i64);
                                        state.cooldown_until_ts = None;
                                    } else if retry_domain == RetryDomain::Probe {
                                        state.cooldown_until_ts =
                                            Some(now_ts.saturating_add(runtime_settings.probe_cooldown_secs));
                                        state.next_allowed_at_ts = state.cooldown_until_ts.unwrap_or(now_ts);
                                    }
                                    state.clone()
                                };
                                state_bundle.touch(now_ts);

                                (state_after_update, state_bundle.clone())
                            };

                            let attempts = state_after_update.attempts;

                            if attempts >= max_attempts {
                                cycle_failed_tasks = cycle_failed_tasks.saturating_add(1);
                                cycle_last_error = Some(sanitize_sensitive_info(e.message()).into_owned());
                                self.scheduled_requeues.remove(&current_key);
                                if retry_domain == RetryDomain::Probe {
                                    remove_current_task = true;
                                    metadata_persist_state = Some(Some(state_bundle_after_update.clone()));
                                    let cooldown_until = state_after_update
                                        .cooldown_until_ts
                                        .map_or_else(|| "none".to_string(), |ts| ts.to_string());
                                    debug!(
                                        "Metadata-[Task] Probe task exhausted (max attempts reached) for input {}: {} (attempts={}/{}, cooldown_until={}, error={})",
                                        input_name,
                                        task_for_execution,
                                        state_after_update.attempts,
                                        max_attempts,
                                        cooldown_until,
                                        e.message()
                                    );
                                } else {
                                    let cooldown_until_ts =
                                        now_ts.saturating_add(runtime_settings.resolve_exhaustion_reset_gap_secs);
                                    let state_bundle_after_update = {
                                        let state_bundle = self.retry_states.entry(current_key.clone()).or_default();
                                        let state = state_bundle.get_mut_or_insert(RetryDomain::Resolve);
                                        state.attempts = max_attempts;
                                        state.next_allowed_at_ts = cooldown_until_ts;
                                        state.cooldown_until_ts = Some(cooldown_until_ts);
                                        state.last_error = Some(e.message().to_string());
                                        state_bundle.touch(now_ts);
                                        state_bundle.clone()
                                    };
                                    self.set_resolve_enqueue_suppression(&current_key, cooldown_until_ts);
                                    self.resolve_exhausted.insert(current_key.clone(), now_ts);
                                    metadata_persist_state = Some(Some(state_bundle_after_update));
                                    remove_current_task = true;
                                    debug!(
                                        "[Metadata-Task] Resolve task exhausted (max attempts reached) for input {}: {} (attempts={}/{}, cooldown_duration={}s, error={})",
                                        input_name,
                                        task_for_execution,
                                        attempts,
                                        max_attempts,
                                        runtime_settings.resolve_exhaustion_reset_gap_secs,
                                        e.message()
                                    );
                                }
                            } else {
                                schedule_requeue_at_ts = Some(state_after_update.next_allowed_at_ts);
                                metadata_persist_state = Some(Some(state_bundle_after_update));
                                if retry_domain == RetryDomain::Resolve {
                                    self.set_resolve_enqueue_suppression(
                                        &current_key,
                                        state_after_update.next_allowed_at_ts,
                                    );
                                }
                                debug!(
                                    "[Metadata-Task] Task failed, scheduling retry for input {}: {} (attempt={}/{}, next_allowed_at={}, backoff={}s, error={})",
                                    input_name,
                                    task_for_execution,
                                    attempts,
                                    max_attempts,
                                    state_after_update.next_allowed_at_ts,
                                    state_after_update.next_allowed_at_ts.saturating_sub(now_ts),
                                    e.message()
                                );
                            }
                        }
                    }
                }
            }

            if let Some(state) = metadata_persist_state {
                self.dirty_retry_state_keys.insert(current_key.clone());
                self.persist_metadata_retry_state(&current_key, state.as_ref()).await;
            }

            // Check and flush batch
            if self.batch_buffer.should_flush() {
                self.release_db_handles();
                let Some(_pause_guard) = self.wait_for_update_pause_window().await else {
                    break;
                };
                Self::flush_batch_static(&input_name, bound_ctx.as_ref(), &mut self.batch_buffer).await;
            }

            if let Some(retry_at_ts) = schedule_requeue_at_ts {
                debug!(
                    "[Task] Scheduling requeue for input {}: {:?} (retry_at_ts={}, in={}s)",
                    input_name,
                    current_key,
                    retry_at_ts,
                    retry_at_ts.saturating_sub(chrono::Utc::now().timestamp())
                );
                self.schedule_requeue_at(current_key.clone(), retry_at_ts);
            }

            if remove_current_task {
                self.finalize_processed_task_success(&current_key, current_generation, &input_name).await;
            }

            if apply_rate_limit {
                // Rate limiting
                if delay_secs > 0
                    && Self::sleep_or_cancel(&self.cancel_token, Duration::from_secs(u64::from(delay_secs))).await
                {
                    break;
                }
            }

            if current_retry_domain == RetryDomain::Resolve {
                consecutive_resolve_tasks = consecutive_resolve_tasks.saturating_add(1);
            } else {
                consecutive_resolve_tasks = 0;
            }

            // Try to get the next task immediately to keep locks open.
            // Ignore phantom signals (channel key without pending map entry).
            if next_task.is_none() {
                if self.cancel_token.is_cancelled() {
                    break;
                }
                while let Ok(key) = self.receiver.try_recv() {
                    if self.cancel_token.is_cancelled() {
                        break;
                    }
                    if let Some(snapshot) = self.load_task_snapshot(key) {
                        next_task = Some(snapshot);
                        break;
                    }
                }
            }

            let channel_has_work = next_task.is_some() || !self.receiver.is_empty();
            let queue_completely_empty =
                !channel_has_work && self.pending_tasks.is_empty() && self.scheduled_requeues.is_empty();

            // Avoid O(n) queue scans per task; report queue status periodically.
            if (channel_has_work || !self.pending_tasks.is_empty())
                && last_queue_log_at.elapsed() >= runtime_settings.queue_log_interval
            {
                let queue_counts = Self::queue_resolve_counts(&self.pending_tasks);
                debug!("In queue to resolve vod: {}, series: {} (input: {input_name})", queue_counts.0, queue_counts.1);
                last_queue_log_at = Instant::now();
            }

            // If no immediate work is available, flush buffered results now even when delayed retries remain pending.
            if !channel_has_work && !self.batch_buffer.is_empty() {
                self.release_db_handles();
                let Some(_pause_guard) = self.wait_for_update_pause_window().await else {
                    break;
                };
                Self::flush_batch_static(&input_name, bound_ctx.as_ref(), &mut self.batch_buffer).await;
            }

            if queue_cycle_active && queue_completely_empty {
                self.last_cycle_completed_at_ts = Some(chrono::Utc::now().timestamp());
                queue_cycle_active = false;
                processed_vod_count = 0;
                processed_series_count = 0;
                consecutive_resolve_tasks = 0;
                if cycle_had_changes {
                    info!("All pending metadata resolves completed for input {input_name} (with changes)");
                    if let Some(ctx) = bound_ctx.clone() {
                        ctx.events.emit(EventMessage::InputMetadataUpdatesCompleted(input_name.clone()));
                    }
                } else {
                    debug!("All pending metadata resolves completed for input {input_name} (no changes, skipping playlist update trigger)");
                }
                // Reported alongside the completion rather than instead of it:
                // a cycle can both produce changes and exhaust tasks, and
                // suppressing the completion would also suppress the playlist
                // update it triggers.
                if cycle_failed_tasks > 0 {
                    warn!(
                        "Metadata update cycle for input {input_name} exhausted {cycle_failed_tasks} task(s) without resolving them"
                    );
                    if let Some(ctx) = bound_ctx.clone() {
                        ctx.events.emit(EventMessage::InputMetadataUpdatesFailed(MetadataUpdateFailure::new(
                            input_name.clone(),
                            cycle_failed_tasks,
                            cycle_had_changes,
                            cycle_last_error.clone(),
                        )));
                    }
                }
                cycle_had_changes = false;
                cycle_failed_tasks = 0;
                cycle_last_error = None;
            }
        }

        // Final flush
        self.release_db_handles();
        if !self.batch_buffer.is_empty() {
            if let Some(_pause_guard) = self.wait_for_update_pause_window().await {
                Self::flush_batch_static(&input_name, bound_ctx.as_ref(), &mut self.batch_buffer).await;
            }
        }
        self.flush_dirty_retry_states().await;

        debug!("Metadata worker stopped for input {input_name}");
    }

    async fn recv_task_fast_or_wait(
        &mut self,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> Option<(TaskKey, UpdateTask, u64)> {
        if self.cancel_token.is_cancelled() {
            return None;
        }

        // Fast path: drain immediate signals until we find a real pending task.
        loop {
            if self.cancel_token.is_cancelled() {
                return None;
            }
            match self.receiver.try_recv() {
                Ok(key) => {
                    if self.cancel_token.is_cancelled() {
                        return None;
                    }
                    if let Some(snapshot) = self.load_task_snapshot(key) {
                        return Some(snapshot);
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return None;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    break;
                }
            }
        }

        // When idle, release read handles to avoid writer starvation.
        self.release_db_handles();

        loop {
            tokio::select! {
                biased;
                () = self.cancel_token.cancelled() => return None,
                res = tokio::time::timeout(Duration::from_secs(runtime_settings.worker_idle_timeout_secs), self.receiver.recv()) => {
                    match res {
                        Ok(Some(key)) => {
                            if self.cancel_token.is_cancelled() {
                                return None;
                            }
                            if let Some(snapshot) = self.load_task_snapshot(key) {
                                return Some(snapshot);
                            }
                        }
                        Ok(None) => return None,
                        Err(_) => {
                            loop {
                                if self.cancel_token.is_cancelled() {
                                    return None;
                                }
                                match self.receiver.try_recv() {
                                    Ok(key) => {
                                        if self.cancel_token.is_cancelled() {
                                            return None;
                                        }
                                        if let Some(snapshot) = self.load_task_snapshot(key) {
                                            return Some(snapshot);
                                        }
                                    }
                                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
                                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                                }
                            }
                            if self.pending_tasks.is_empty()
                                && self.receiver.is_empty()
                                && self.scheduled_requeues.is_empty()
                            {
                                return None;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn wait_for_update_pause_window(&mut self) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        if let Ok(guard) = self.update_pause_gate.clone().try_read_owned() {
            return Some(guard);
        }

        // Foreground writer is active or queued. Release cached handles before waiting to avoid AB-BA patterns.
        self.release_db_handles();
        tokio::select! {
            () = self.cancel_token.cancelled() => None,
            guard = self.update_pause_gate.clone().read_owned() => {
                Some(guard)
            }
        }
    }

    fn release_db_handles(&mut self) {
        if !self.db_handles.is_empty() {
            self.db_handles.clear();
        }
        if !self.failed_clusters.is_empty() {
            self.failed_clusters.clear();
        }
    }

    async fn sleep_or_cancel(cancel_token: &CancellationToken, duration: Duration) -> bool {
        tokio::select! {
            () = cancel_token.cancelled() => true,
            () = tokio::time::sleep(duration) => false,
        }
    }

    // Three associated functions generated by the `collect_virtual_updates!`
    // macro below. The macro body walks a single `BatchResultCollector` field
    // and builds a `HashMap<virtual_id, &Props>` of pending per-virtual updates.
    collect_virtual_updates!(collect_vod_virtual_updates, vod, VideoStreamProperties, PlaylistItemType::Video);
    collect_virtual_updates!(
        collect_series_virtual_updates,
        series,
        SeriesStreamProperties,
        PlaylistItemType::SeriesInfo
    );
    collect_virtual_updates!(collect_live_virtual_updates, live, LiveStreamProperties, PlaylistItemType::Live);

    apply_cascade_updates!(apply_vod_cascade_updates, VideoStreamProperties, XtreamCluster::Video, Video, "VOD");
    apply_cascade_updates!(
        apply_series_cascade_updates,
        SeriesStreamProperties,
        XtreamCluster::Series,
        Series,
        "Series"
    );
    apply_cascade_updates!(apply_live_cascade_updates, LiveStreamProperties, XtreamCluster::Live, Live, "Live");
}
