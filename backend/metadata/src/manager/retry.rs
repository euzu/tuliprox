use super::{
    load_metadata_retry_states_from_disk, persist_metadata_retry_state_to_disk, spawn_blocking_limited, InputWorker,
    MetadataUpdateManager, MetadataUpdateRuntimeSettings, ScopedTaskKey, TaskKey, METADATA_RETRY_STATE_FILE,
    RETRY_STATE_MIN_TTL_SECS, RETRY_STATE_PRUNE_INTERVAL_SECS, TASK_ERR_NO_CONNECTION, TASK_ERR_PREEMPTED,
    TASK_ERR_UPDATE_IN_PROGRESS,
};
use crate::ctx::MetadataUpdateCtx;
use log::warn;
use shared::model::EventSink;
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tuliprox_core::{model::UpdateTask, utils::debug_if_enabled};
use tuliprox_repository::get_input_storage_path;

#[derive(Debug, Clone)]
pub(super) struct RetryState {
    pub(super) attempts: u8,
    pub(super) next_allowed_at_ts: i64,
    pub(super) cooldown_until_ts: Option<i64>,
    pub(super) last_error: Option<String>,
    pub(super) source_last_modified: Option<u64>,
}

impl RetryState {
    pub(super) fn new() -> Self {
        Self {
            attempts: 0,
            next_allowed_at_ts: 0,
            cooldown_until_ts: None,
            last_error: None,
            source_last_modified: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryDomain {
    Resolve,
    Probe,
    Tmdb,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TaskRetryState {
    pub(super) resolve: Option<RetryState>,
    pub(super) probe: Option<RetryState>,
    pub(super) tmdb: Option<RetryState>,
    pub(super) updated_at_ts: i64,
}

impl TaskRetryState {
    pub(super) fn is_empty(&self) -> bool { self.resolve.is_none() && self.probe.is_none() && self.tmdb.is_none() }

    pub(super) fn touch(&mut self, now_ts: i64) { self.updated_at_ts = now_ts.max(1); }

    pub(super) fn max_domain_timestamp(&self) -> i64 {
        let domain_max = |state: &RetryState| state.next_allowed_at_ts.max(state.cooldown_until_ts.unwrap_or(0));
        self.resolve
            .as_ref()
            .map_or(0, domain_max)
            .max(self.probe.as_ref().map_or(0, domain_max))
            .max(self.tmdb.as_ref().map_or(0, domain_max))
    }

    fn is_stale(&self, now_ts: i64, ttl_secs: i64) -> bool {
        let ttl_secs = ttl_secs.max(1);
        let anchor_ts = self.updated_at_ts.max(self.max_domain_timestamp());
        now_ts >= anchor_ts.saturating_add(ttl_secs)
    }

    pub(super) fn get(&self, domain: RetryDomain) -> Option<&RetryState> {
        match domain {
            RetryDomain::Resolve => self.resolve.as_ref(),
            RetryDomain::Probe => self.probe.as_ref(),
            RetryDomain::Tmdb => self.tmdb.as_ref(),
        }
    }

    pub(super) fn get_mut_or_insert(&mut self, domain: RetryDomain) -> &mut RetryState {
        let slot = match domain {
            RetryDomain::Resolve => &mut self.resolve,
            RetryDomain::Probe => &mut self.probe,
            RetryDomain::Tmdb => &mut self.tmdb,
        };
        slot.get_or_insert_with(RetryState::new)
    }

    pub(super) fn clear_domain(&mut self, domain: RetryDomain) {
        match domain {
            RetryDomain::Resolve => self.resolve = None,
            RetryDomain::Probe => self.probe = None,
            RetryDomain::Tmdb => self.tmdb = None,
        }
    }
}

impl MetadataUpdateManager {
    pub(super) fn prune_resolve_enqueue_suppressions(&self, now_ts: i64) {
        let last_pruned_at = self.last_resolve_enqueue_suppression_prune_at_ts.load(Ordering::Relaxed);
        if last_pruned_at != 0 && now_ts.saturating_sub(last_pruned_at) < RETRY_STATE_PRUNE_INTERVAL_SECS {
            return;
        }
        self.last_resolve_enqueue_suppression_prune_at_ts.store(now_ts, Ordering::Relaxed);
        self.resolve_enqueue_suppressions.retain(|_, suppressed_until_ts| *suppressed_until_ts > now_ts);
    }
}

impl InputWorker {
    pub(super) async fn ensure_metadata_retry_state_loaded<E: EventSink + Clone + 'static>(
        &mut self,
        input_name: &str,
        bound_ctx: Option<&MetadataUpdateCtx<E>>,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) {
        if self.metadata_retry_loaded {
            return;
        }
        let now_ts = chrono::Utc::now().timestamp();
        if self.metadata_retry_load_retry_at_ts.is_some_and(|retry_at_ts| now_ts < retry_at_ts) {
            return;
        }

        let Some(ctx) = bound_ctx else {
            self.metadata_retry_load_retry_at_ts =
                Some(now_ts.saturating_add(runtime_settings.metadata_retry_load_retry_delay_secs));
            return;
        };

        let storage_dir = ctx.app_config.config.load().storage_dir.clone();
        let Ok(storage_path) = get_input_storage_path(input_name, &storage_dir).await else {
            warn!("Could not resolve storage path for metadata retry state on input {input_name}");
            self.metadata_retry_load_retry_at_ts =
                Some(now_ts.saturating_add(runtime_settings.metadata_retry_load_retry_delay_secs));
            return;
        };

        let retry_path = storage_path.join(METADATA_RETRY_STATE_FILE);
        self.metadata_retry_state_path = Some(retry_path.clone());

        let loaded = spawn_blocking_limited(move || load_metadata_retry_states_from_disk(&retry_path))
            .await
            .map_err(|err| err.to_string())
            .and_then(|result| result.map_err(|err| err.to_string()));

        let loaded = match loaded {
            Ok(states) => states,
            Err(err) => {
                warn!("Failed to load metadata retry state for input {input_name}: {err}");
                self.metadata_retry_load_retry_at_ts =
                    Some(now_ts.saturating_add(runtime_settings.metadata_retry_load_retry_delay_secs));
                return;
            }
        };

        // Intentionally do not resurrect pending tasks solely from persisted retry state.
        // The persisted state is applied once the corresponding task is naturally queued again
        // (for example by the next playlist update), because state alone does not contain the
        // full `UpdateTask` payload for all variants.
        for (key, state) in loaded {
            self.sync_resolve_enqueue_suppression_from_retry_state(&key, &state, now_ts);
            self.retry_states.insert(key, state);
        }
        self.prune_retry_tracking_maps(now_ts, runtime_settings).await;
        self.last_retry_state_prune_at_ts = Some(now_ts);
        self.metadata_retry_loaded = true;
        self.metadata_retry_load_retry_at_ts = None;
    }

    pub(super) async fn persist_metadata_retry_state(&mut self, key: &TaskKey, state: Option<&TaskRetryState>) {
        let Some(path) = self.metadata_retry_state_path.clone() else {
            return;
        };
        if !self.dirty_retry_state_keys.contains(key) {
            return;
        }

        let key_for_persist = key.clone();
        let key_for_dirty = key.clone();
        let state = state.cloned();
        let input_name = self.input_name.clone();
        let persist_result = spawn_blocking_limited(move || {
            persist_metadata_retry_state_to_disk(&path, &key_for_persist, state.as_ref())
        })
        .await;

        match persist_result {
            Ok(Ok(())) => {
                self.dirty_retry_state_keys.remove(&key_for_dirty);
            }
            Ok(Err(err)) => warn!("Failed to persist metadata retry state for input {input_name}: {err}"),
            Err(err) => warn!("Failed to persist metadata retry state for input {input_name}: {err}"),
        }
    }

    pub(super) async fn flush_dirty_retry_states(&mut self) {
        if self.dirty_retry_state_keys.is_empty() {
            return;
        }

        let dirty_keys: Vec<TaskKey> = self.dirty_retry_state_keys.iter().cloned().collect();
        for key in dirty_keys {
            let state = self.retry_states.get(&key).cloned();
            self.persist_metadata_retry_state(&key, state.as_ref()).await;
        }
    }

    pub(super) fn schedule_requeue_at(&self, key: TaskKey, retry_at_ts: i64) {
        let now_ts = chrono::Utc::now().timestamp();
        let retry_at_ts = retry_at_ts.max(now_ts);

        if self.scheduled_requeues.get(&key).is_some_and(|existing| *existing == retry_at_ts) {
            return;
        }

        self.scheduled_requeues.insert(key.clone(), retry_at_ts);

        let sender = self.sender.clone();
        let pending_tasks = Arc::clone(&self.pending_tasks);
        let pending_task_count = Arc::clone(&self.pending_task_count);
        let scheduled = Arc::clone(&self.scheduled_requeues);
        let cancel_token = self.cancel_token.clone();
        let input_name = self.input_name.clone();

        tokio::spawn(async move {
            let delay_secs = retry_at_ts.saturating_sub(chrono::Utc::now().timestamp());
            if delay_secs > 0 {
                let delay = Duration::from_secs(u64::try_from(delay_secs).unwrap_or(u64::MAX));
                tokio::select! {
                    () = cancel_token.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
            }

            let should_send = scheduled.get(&key).is_some_and(|scheduled_at| *scheduled_at == retry_at_ts);
            if !should_send {
                return;
            }
            scheduled.remove(&key);

            if !pending_tasks.contains_key(&key) {
                return;
            }

            if sender.send(key.clone()).await.is_err() {
                if pending_tasks.remove(&key).is_some() {
                    MetadataUpdateManager::decrement_pending_task_count(&pending_task_count);
                }
                warn!("Failed to schedule delayed retry task for input {input_name}");
            }
        });
    }

    pub(super) async fn finalize_processed_task_success(
        &mut self,
        current_key: &TaskKey,
        current_generation: u64,
        input_name: &str,
    ) -> bool {
        // Atomically remove processed key and reinsert it if it changed while in-flight.
        if let Some((_k, removed_task)) = self.pending_tasks.remove(current_key) {
            let latest_generation = removed_task.generation.load(Ordering::Relaxed);
            if latest_generation != current_generation {
                self.pending_tasks.insert(current_key.clone(), removed_task);
                if self.sender.send(current_key.clone()).await.is_err() {
                    if self.pending_tasks.remove(current_key).is_some() {
                        MetadataUpdateManager::decrement_pending_task_count(&self.pending_task_count);
                    }
                    warn!("Failed to schedule merged task replay for input {input_name}");
                    return false;
                }
                return true;
            }
            // Task finished and is not reinserted.
            MetadataUpdateManager::decrement_pending_task_count(&self.pending_task_count);
        }
        false
    }

    fn retry_state_ttl_secs(runtime_settings: &MetadataUpdateRuntimeSettings) -> i64 {
        let max_resolve_backoff = i64::try_from(runtime_settings.max_resolve_retry_backoff_secs).unwrap_or(i64::MAX);
        runtime_settings
            .tmdb_cooldown_secs
            .max(runtime_settings.probe_cooldown_secs)
            .max(runtime_settings.resolve_exhaustion_reset_gap_secs)
            .max(max_resolve_backoff)
            .saturating_mul(6)
            .max(RETRY_STATE_MIN_TTL_SECS)
    }

    pub(super) async fn prune_retry_tracking_maps_if_needed(
        &mut self,
        now_ts: i64,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) {
        let due = self
            .last_retry_state_prune_at_ts
            .is_none_or(|last| now_ts.saturating_sub(last) >= RETRY_STATE_PRUNE_INTERVAL_SECS);
        if !due {
            return;
        }
        self.last_retry_state_prune_at_ts = Some(now_ts);
        self.prune_retry_tracking_maps(now_ts, runtime_settings).await;
    }

    async fn prune_retry_tracking_maps(&mut self, now_ts: i64, runtime_settings: &MetadataUpdateRuntimeSettings) {
        let retry_state_ttl_secs = Self::retry_state_ttl_secs(runtime_settings);
        let resolve_exhausted_ttl_secs = Self::resolve_exhausted_ttl_secs(runtime_settings);
        let stale_retry_keys: Vec<TaskKey> = self
            .retry_states
            .iter()
            .filter_map(
                |(key, state)| {
                    if state.is_stale(now_ts, retry_state_ttl_secs) {
                        Some(key.clone())
                    } else {
                        None
                    }
                },
            )
            .collect();

        for key in stale_retry_keys {
            if self.retry_states.remove(&key).is_some() {
                self.clear_resolve_enqueue_suppression(&key);
                self.clear_tmdb_source_marker(&key);
                self.dirty_retry_state_keys.insert(key.clone());
                debug_if_enabled!("Pruned stale metadata retry state for input {}: {:?}", self.input_name, key);
                self.persist_metadata_retry_state(&key, None).await;
            }
        }

        let stale_resolve_exhausted_keys: Vec<TaskKey> = self
            .resolve_exhausted
            .iter()
            .filter_map(|(key, exhausted_at_ts)| {
                if now_ts.saturating_sub(*exhausted_at_ts) >= resolve_exhausted_ttl_secs {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();
        for key in stale_resolve_exhausted_keys {
            self.resolve_exhausted.remove(&key);
            self.clear_resolve_enqueue_suppression(&key);
        }

        let no_change_ttl = Duration::from_secs(runtime_settings.no_change_cache_ttl_secs);
        self.recently_completed_no_change.retain(|_, (completed_at, _)| completed_at.elapsed() < no_change_ttl);
    }

    pub(super) fn compute_resolve_retry_backoff_secs(
        base_delay_secs: u16,
        attempts: u8,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> u64 {
        let base_delay = u64::from(base_delay_secs).max(runtime_settings.resolve_min_retry_base_secs);
        let exp = u32::from(attempts.saturating_sub(1).min(6));
        let without_jitter =
            base_delay.saturating_mul(2_u64.saturating_pow(exp)).min(runtime_settings.max_resolve_retry_backoff_secs);
        Self::apply_jitter(without_jitter, runtime_settings.backoff_jitter_percent)
    }

    pub(super) fn compute_retry_delay_secs(
        base_delay_secs: u16,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> u64 {
        u64::from(base_delay_secs).max(runtime_settings.retry_delay_secs)
    }

    pub(super) fn compute_probe_retry_backoff_secs(
        attempts: u8,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> u64 {
        let base_secs = match attempts {
            1 => runtime_settings.probe_retry_backoff_step_1_secs,
            2 => runtime_settings.probe_retry_backoff_step_2_secs,
            _ => runtime_settings.probe_retry_backoff_step_3_secs,
        };
        Self::apply_jitter(base_secs, runtime_settings.backoff_jitter_percent)
    }

    fn apply_jitter(base_secs: u64, jitter_percent: u8) -> u64 {
        let jitter_percent = i64::from(jitter_percent);
        let jitter_percent = fastrand::i64(-jitter_percent..=jitter_percent);
        let base_i64 = i64::try_from(base_secs).unwrap_or(i64::MAX);
        let jitter_delta = base_i64.saturating_mul(jitter_percent).saturating_div(100);
        let jittered = base_i64.saturating_add(jitter_delta);
        u64::try_from(jittered.max(1)).unwrap_or(1)
    }

    #[inline]
    pub(super) fn retry_domain_for_task(task: &UpdateTask) -> RetryDomain {
        if Self::is_probe_task(task) || Self::is_probe_only_resolve_task(task) {
            RetryDomain::Probe
        } else {
            RetryDomain::Resolve
        }
    }

    pub(super) fn should_skip_recent_no_change_task(
        &mut self,
        current_key: &TaskKey,
        task_for_execution: &UpdateTask,
        runtime_settings: &MetadataUpdateRuntimeSettings,
    ) -> bool {
        let Some((completed_at, cached_reasons)) = self.recently_completed_no_change.get(current_key).copied() else {
            return false;
        };

        let ttl = Duration::from_secs(runtime_settings.no_change_cache_ttl_secs);
        let current_reasons = Self::task_reason(task_for_execution);
        if completed_at.elapsed() < ttl && current_reasons == cached_reasons {
            self.scheduled_requeues.remove(current_key);
            return true;
        }

        self.recently_completed_no_change.remove(current_key);
        false
    }

    pub(super) fn set_resolve_enqueue_suppression(&self, current_key: &TaskKey, until_ts: i64) {
        let scoped_key = ScopedTaskKey::new(self.input_name.clone(), current_key.clone());
        self.resolve_enqueue_suppressions.insert(scoped_key, until_ts);
    }

    pub(super) fn clear_resolve_enqueue_suppression(&self, current_key: &TaskKey) {
        let scoped_key = ScopedTaskKey::new(self.input_name.clone(), current_key.clone());
        self.resolve_enqueue_suppressions.remove(&scoped_key);
    }

    fn sync_resolve_enqueue_suppression_from_retry_state(
        &self,
        current_key: &TaskKey,
        state: &TaskRetryState,
        now_ts: i64,
    ) {
        let Some(resolve_state) = state.resolve.as_ref() else {
            self.clear_resolve_enqueue_suppression(current_key);
            return;
        };

        let suppressed_until_ts = resolve_state.cooldown_until_ts.unwrap_or(resolve_state.next_allowed_at_ts);
        if suppressed_until_ts > now_ts {
            self.set_resolve_enqueue_suppression(current_key, suppressed_until_ts);
        } else {
            self.clear_resolve_enqueue_suppression(current_key);
        }
    }

    pub(super) fn is_transient_worker_error(message: &str) -> bool {
        message == TASK_ERR_UPDATE_IN_PROGRESS || message == TASK_ERR_PREEMPTED || message == TASK_ERR_NO_CONNECTION
    }

    #[inline]
    pub(super) fn is_permanent_not_found_error(message: &str) -> bool {
        let normalized = message.to_ascii_lowercase();
        Self::contains_standalone_fragment(&normalized, "404")
            || Self::contains_standalone_fragment(&normalized, "not found")
    }

    #[inline]
    fn is_word_byte(byte: u8) -> bool { byte.is_ascii_alphanumeric() || byte == b'_' }

    fn contains_standalone_fragment(haystack: &str, fragment: &str) -> bool {
        if fragment.is_empty() || haystack.len() < fragment.len() {
            return false;
        }

        let bytes = haystack.as_bytes();
        let mut search_from = 0usize;

        while let Some(relative_idx) = haystack[search_from..].find(fragment) {
            let start = search_from + relative_idx;
            let end = start + fragment.len();
            let before_is_word = start > 0 && Self::is_word_byte(bytes[start - 1]);
            let after_is_word = end < bytes.len() && Self::is_word_byte(bytes[end]);

            if !before_is_word && !after_is_word {
                return true;
            }
            search_from = end;
        }

        false
    }
}
