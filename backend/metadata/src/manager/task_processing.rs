use super::{
    spawn_blocking_limited, DbHandle, InputWorker, RetryDomain, TaskKey, PROBE_TASK_TIMEOUT_SECS,
    TASK_ERR_NO_CONNECTION, TASK_ERR_PREEMPTED, TASK_ERR_UPDATE_IN_PROGRESS,
};
use crate::ctx::MetadataUpdateCtx;
use log::{debug, error, warn};
use shared::{
    defaults::default_probe_user_priority,
    error::TuliproxError,
    model::{EventSink, InputType, PlaylistItemType, StreamProperties, XtreamCluster, XtreamPlaylistItem},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tuliprox_core::{
    model::{BatchResultCollector, ProviderHandle, ProviderIdType, ResolveReason, UpdateTask},
    utils::debug_if_enabled,
};
use tuliprox_processing::processor::{
    probe_generic_stream_metadata, update_generic_stream_metadata, update_live_stream_metadata, update_properties,
    update_series_metadata, update_vod_metadata, GenericProbeMetadataOutcome, GenericProbeOutcome, SeriesProbeSettings,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct ProcessTaskOutcome {
    pub(super) task_changed: bool,
    pub(super) tmdb_pending: bool,
    pub(super) probe_pending: bool,
}

impl InputWorker {
    pub(super) fn load_task_snapshot(&self, key: TaskKey) -> Option<(TaskKey, UpdateTask, u64)> {
        let entry = self.pending_tasks.get(&key)?;
        let generation = entry.generation.load(Ordering::Relaxed);
        let task = entry.task.lock().clone();
        Some((key, task, generation))
    }

    pub(super) fn take_pending_probe_task_snapshot(&self) -> Option<(TaskKey, UpdateTask, u64)> {
        for entry in self.pending_tasks.iter() {
            if self.scheduled_requeues.contains_key(entry.key()) {
                continue;
            }

            let generation = entry.generation.load(Ordering::Relaxed);
            let task = entry.task.lock().clone();
            if Self::retry_domain_for_task(&task) == RetryDomain::Probe {
                return Some((entry.key().clone(), task, generation));
            }
        }

        None
    }

    // Helper for get_item_name with caching
    async fn get_item_name_static<E: EventSink + Clone + 'static>(
        input_name: &str,
        ctx: &MetadataUpdateCtx<E>,
        task: &UpdateTask,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Option<String> {
        let (id, cluster) = match task {
            UpdateTask::ResolveVod { id, .. } => (id, XtreamCluster::Video),
            UpdateTask::ResolveSeries { id, .. } => (id, XtreamCluster::Series),
            UpdateTask::ProbeLive { id, .. } => (id, XtreamCluster::Live),
            UpdateTask::ProbeStream { .. } => return None,
        };

        if let ProviderIdType::Id(vid) = id {
            let stream_id = *vid;
            if let Some(query) = Self::get_or_open_query(input_name, ctx, cluster, db_handles, failed_clusters).await {
                let query = Arc::clone(&query);
                let item = match spawn_blocking_limited(move || {
                    let mut guard = query.lock();
                    guard.query_zero_copy(&stream_id).ok().flatten()
                })
                .await
                {
                    Ok(item) => item,
                    Err(err) => {
                        error!("Failed to query item name for {stream_id}: {err}");
                        None
                    }
                };

                if let Some(item) = item {
                    return Some(if item.title.is_empty() { item.name.to_string() } else { item.title.to_string() });
                }
            }
        }
        None
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn process_task_static<E: EventSink + Clone + 'static>(
        input_name: &Arc<str>,
        bound_ctx: Option<&MetadataUpdateCtx<E>>,
        task: &UpdateTask,
        collector: &mut BatchResultCollector,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Result<ProcessTaskOutcome, TuliproxError> {
        let ctx =
            bound_ctx.ok_or_else(|| shared::error::TuliproxError::Config("metadata context not bound".to_string()))?;

        let Some(input_base) = ctx.app_config.get_input_by_name(input_name) else {
            return Err(shared::error::TuliproxError::Config(format!("Input {input_name} not found")));
        };

        if !input_base.enabled {
            return Err(shared::error::TuliproxError::Config(format!("Input {input_name} is disabled")));
        }

        // Background metadata/probe tasks are low-priority.
        // Never run them while a foreground playlist update is active.
        if let Some(guard) = ctx.update_guard.try_playlist() {
            drop(guard);
        } else {
            return Err(shared::error::TuliproxError::Config(TASK_ERR_UPDATE_IN_PROGRESS.to_string()));
        }

        let needs_probe_connection = Self::task_needs_provider_connection(task, input_base.input_type);

        let probe_priority = ctx
            .app_config
            .config
            .load()
            .metadata_update
            .as_ref()
            .map_or(default_probe_user_priority(), |cfg| cfg.probe.user_priority);

        // Reserve provider capacity only for actual probe work (ffprobe paths).
        let provider_handle = if needs_probe_connection {
            let Some(handle) = ctx.active_provider.acquire_connection_for_probe(input_name, probe_priority) else {
                debug_if_enabled!("No provider connection available for background task {}, skipping...", task);
                return Err(shared::error::TuliproxError::Config(TASK_ERR_NO_CONNECTION.to_string()));
            };
            Some(handle)
        } else {
            None
        };

        let item_title = Self::get_item_name_static(input_name, ctx, task, db_handles, failed_clusters).await;

        let config_to_use = provider_handle.as_ref().and_then(|handle| handle.allocation.get_provider_config());
        let name_display = item_title.as_deref().map_or(String::new(), |n| format!(" \"{n}\""));

        debug!("Processing task for {input_name}: {task}{name_display}");

        let pre_vod_updates = collector.vod.len();
        let pre_series_updates = collector.series.len();
        let pre_live_updates = collector.live.len();

        // Determine input to use (may be alias)
        let input_to_use = config_to_use
            .filter(|alloc| alloc.name != input_base.name)
            .and_then(|alloc| input_base.aliases.as_ref()?.iter().find(|a| a.enabled && a.name == alloc.name))
            .map(|alias_def| {
                let mut temp_input = (*input_base).clone();
                temp_input.url.clone_from(&alias_def.url);
                temp_input.username.clone_from(&alias_def.username);
                temp_input.password.clone_from(&alias_def.password);
                Arc::new(temp_input)
            })
            .unwrap_or(input_base);

        let client = if tuliprox_core::model::should_use_manual_redirects(&ctx.app_config) {
            ctx.http_clients.no_redirect.load()
        } else {
            ctx.http_clients.default.load()
        };

        // Execute task; probe tasks get a reserved provider handle, resolve tasks don't.
        // The hard timeout is applied only to tasks that perform network probing — Probe*
        // variants and Resolve tasks whose reason includes ResolveReason::Probe — because
        // those are the only ones that can stall on an unresponsive provider.  Pure metadata
        // resolve tasks (Info / TMDB / Date only) run without a timeout so they are not
        // subject to the probe hard limit.
        let exec_fut = async {
            if let Some(handle) = provider_handle.as_ref() {
                if let Some(token) = &handle.cancel_token {
                    tokio::select! {
                        biased;

                        () = token.cancelled() => {
                            debug_if_enabled!("Metadata update task preempted by user request for input {}", input_name);
                            Err(shared::error::TuliproxError::Config(TASK_ERR_PREEMPTED.to_string()))
                        }

                        res = Self::execute_task_inner_static(ctx, &client, &input_to_use, task, item_title.as_deref(), Some(handle), probe_priority, collector, db_handles, failed_clusters) => {
                            res
                        }
                    }
                } else {
                    Self::execute_task_inner_static(
                        ctx,
                        &client,
                        &input_to_use,
                        task,
                        item_title.as_deref(),
                        Some(handle),
                        probe_priority,
                        collector,
                        db_handles,
                        failed_clusters,
                    )
                    .await
                }
            } else {
                Self::execute_task_inner_static(
                    ctx,
                    &client,
                    &input_to_use,
                    task,
                    item_title.as_deref(),
                    None,
                    probe_priority,
                    collector,
                    db_handles,
                    failed_clusters,
                )
                .await
            }
        };
        let needs_probe_timeout = Self::is_probe_task(task)
            || (Self::is_resolve_task(task) && Self::task_reason(task).contains(ResolveReason::Probe));
        let res = if needs_probe_timeout {
            match tokio::time::timeout(Duration::from_secs(PROBE_TASK_TIMEOUT_SECS), exec_fut).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    error!(
                        "Metadata probe task timed out after {PROBE_TASK_TIMEOUT_SECS}s for input {input_name}: {task}; \
                         releasing provider handle and skipping task"
                    );
                    Err(shared::error::TuliproxError::Config(format!(
                        "Task timed out after {PROBE_TASK_TIMEOUT_SECS}s for input {input_name}: {task}"
                    )))
                }
            }
        } else {
            exec_fut.await
        };

        if provider_handle.is_some() {
            ctx.connection_manager.release_provider_handle(provider_handle);
        }
        match res {
            Ok((tmdb_and_date_present, probe_pending)) => {
                let task_changed = match task {
                    UpdateTask::ResolveVod { .. } => collector.vod.len() > pre_vod_updates,
                    UpdateTask::ResolveSeries { .. } => collector.series.len() > pre_series_updates,
                    UpdateTask::ProbeLive { .. } => collector.live.len() > pre_live_updates,
                    UpdateTask::ProbeStream { .. } => true,
                };
                let tmdb_pending = match task {
                    UpdateTask::ResolveVod { reason, .. } | UpdateTask::ResolveSeries { reason, .. } => {
                        if reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date) {
                            !tmdb_and_date_present
                        } else {
                            false
                        }
                    }
                    UpdateTask::ProbeLive { .. } | UpdateTask::ProbeStream { .. } => false,
                };
                Ok(ProcessTaskOutcome { task_changed, tmdb_pending, probe_pending })
            }
            Err(e) => Err(e),
        }
    }

    pub(super) fn task_needs_provider_connection(task: &UpdateTask, input_type: InputType) -> bool {
        match task {
            UpdateTask::ProbeLive { .. } => true,
            // Local library and media-server probing must not depend on IPTV provider capacity.
            UpdateTask::ProbeStream { .. } => {
                !(matches!(input_type, InputType::Library) || input_type.is_media_server())
            }
            // Resolve tasks handle their own probe connection acquisition internally.
            // This avoids holding a provider connection for the entire duration of
            // info fetch + TMDB resolve + probe, reducing "provider exhausted" errors.
            UpdateTask::ResolveVod { .. } | UpdateTask::ResolveSeries { .. } => false,
        }
    }

    fn batchable_generic_probe_xtream_cluster(item_type: PlaylistItemType) -> Option<XtreamCluster> {
        if item_type.is_live() {
            Some(XtreamCluster::Live)
        } else if item_type.is_video() {
            Some(XtreamCluster::Video)
        } else {
            None
        }
    }

    pub(super) fn apply_pending_generic_probe_base(
        collector: &BatchResultCollector,
        cluster: XtreamCluster,
        provider_id: u32,
        item: &mut XtreamPlaylistItem,
    ) {
        match cluster {
            XtreamCluster::Video => {
                if let Some((_, props)) = collector
                    .vod
                    .iter()
                    .rev()
                    .find(|(id, _)| matches!(id, ProviderIdType::Id(id) if *id == provider_id))
                {
                    item.additional_properties = Some(StreamProperties::Video(Box::new(props.clone())));
                }
            }
            XtreamCluster::Live => {
                if let Some((_, props)) = collector
                    .live
                    .iter()
                    .rev()
                    .find(|(id, _)| matches!(id, ProviderIdType::Id(id) if *id == provider_id))
                {
                    item.additional_properties = Some(StreamProperties::Live(Box::new(props.clone())));
                }
            }
            XtreamCluster::Series => {}
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn execute_task_inner_static<E: EventSink + Clone + 'static>(
        ctx: &MetadataUpdateCtx<E>,
        client: &reqwest::Client,
        input: &Arc<tuliprox_core::model::ConfigInput>,
        task: &UpdateTask,
        item_title: Option<&str>,
        active_handle: Option<&ProviderHandle>,
        probe_priority: i8,
        collector: &mut BatchResultCollector,
        db_handles: &mut HashMap<XtreamCluster, DbHandle>,
        failed_clusters: &mut HashSet<XtreamCluster>,
    ) -> Result<(bool, bool), TuliproxError> {
        // The returned tuple is `(tmdb_and_date_present, probe_pending)`.
        // `tmdb_and_date_present` avoids false-positive TMDB "no match" cooldowns.
        // `probe_pending` prevents skipped/aborted probes from being cached as no-op.
        match task {
            UpdateTask::ResolveVod { id, reason, .. } => {
                let fetch_info = reason.contains(ResolveReason::Info);
                let resolve_tmdb =
                    fetch_info || reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date);
                let will_probe = reason.contains(ResolveReason::Probe);

                // If we are going to probe, release the cached handle to avoid holding a READ lock
                // for along time (blocks writers) and also to avoid potential deadlocks if
                // the probe function itself tries to acquire a WRITE lock later.
                if will_probe {
                    db_handles.remove(&XtreamCluster::Video);
                }

                let query_opt = if will_probe {
                    None
                } else {
                    Self::get_or_open_query(&input.name, ctx, XtreamCluster::Video, db_handles, failed_clusters).await
                };

                let tmdb_and_date_present = AtomicBool::new(false);
                let probe_pending = AtomicBool::new(false);
                match update_vod_metadata(
                    &ctx.app_config,
                    client,
                    input,
                    id.clone(),
                    active_handle,
                    &ctx.active_provider,
                    item_title,
                    false, // Batch collect
                    fetch_info,
                    resolve_tmdb,
                    will_probe,
                    query_opt,
                    Some(&tmdb_and_date_present),
                    Some(&probe_pending),
                )
                .await
                {
                    Ok(Some(props)) => {
                        collector.add_vod(id.clone(), props);
                        Ok((tmdb_and_date_present.load(Ordering::Relaxed), probe_pending.load(Ordering::Relaxed)))
                    }
                    Ok(None) => {
                        Ok((tmdb_and_date_present.load(Ordering::Relaxed), probe_pending.load(Ordering::Relaxed)))
                    }
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ResolveSeries { id, reason, .. } => {
                let fetch_info = reason.contains(ResolveReason::Info);
                let resolve_tmdb = reason.contains(ResolveReason::Tmdb) || reason.contains(ResolveReason::Date);
                let will_probe = reason.contains(ResolveReason::Probe);
                let series_probe_settings = {
                    let config = ctx.app_config.config.load();
                    SeriesProbeSettings::from_metadata_update(config.metadata_update.as_ref())
                };

                if will_probe {
                    db_handles.remove(&XtreamCluster::Series);
                }

                // Get handle for Series
                let query_opt = if will_probe {
                    None
                } else {
                    Self::get_or_open_query(&input.name, ctx, XtreamCluster::Series, db_handles, failed_clusters).await
                };

                let tmdb_and_date_present = AtomicBool::new(false);
                let probe_pending = AtomicBool::new(false);
                match update_series_metadata(
                    &ctx.app_config,
                    client,
                    input,
                    id.clone(),
                    &ctx.active_provider,
                    active_handle,
                    item_title,
                    false, // Batch collect
                    fetch_info,
                    resolve_tmdb,
                    will_probe,
                    series_probe_settings,
                    query_opt,
                    Some(&tmdb_and_date_present),
                    Some(&probe_pending),
                )
                .await
                {
                    Ok(Some(props)) => {
                        collector.add_series(id.clone(), props);
                        Ok((tmdb_and_date_present.load(Ordering::Relaxed), probe_pending.load(Ordering::Relaxed)))
                    }
                    Ok(None) => {
                        Ok((tmdb_and_date_present.load(Ordering::Relaxed), probe_pending.load(Ordering::Relaxed)))
                    }
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ProbeLive { id, .. } => {
                // ProbeLive always probes, so we must never use a cached handle here.
                db_handles.remove(&XtreamCluster::Live);

                match update_live_stream_metadata(
                    &ctx.app_config,
                    client,
                    input,
                    id.clone(),
                    false,
                    None,
                    active_handle,
                    &ctx.active_provider,
                )
                .await
                {
                    Ok(Some(props)) => {
                        collector.add_live(id.clone(), props);
                        Ok((false, false))
                    }
                    Ok(None) => Ok((false, false)),
                    Err(e) => Err(e),
                }
            }
            UpdateTask::ProbeStream { unique_id, url, item_type, .. } => {
                let task_key = TaskKey::from_task(task);
                let probe_identifier = if unique_id.trim().is_empty() { url.as_str() } else { unique_id.as_str() };

                if input.input_type.is_xtream() {
                    if let Some(cluster) = Self::batchable_generic_probe_xtream_cluster(*item_type) {
                        let Ok(provider_id) = unique_id.parse::<u32>() else {
                            warn!("Skipping xtream generic probe update with non-numeric id: {unique_id}");
                            return Ok((false, false));
                        };

                        let outcome = probe_generic_stream_metadata(
                            &ctx.app_config,
                            client,
                            input.as_ref(),
                            unique_id,
                            url,
                            *item_type,
                            &ctx.active_provider,
                            active_handle,
                            probe_priority,
                            &ctx.events,
                        )
                        .await?;

                        let metadata = match outcome {
                            GenericProbeMetadataOutcome::Metadata(metadata) => metadata,
                            GenericProbeMetadataOutcome::Noop => return Ok((false, false)),
                            GenericProbeMetadataOutcome::ProbeFailed => {
                                return Err(shared::error::TuliproxError::Config(format!(
                                    "Probe stream task failed for key {task_key:?} ({probe_identifier})"
                                )));
                            }
                        };

                        let Some(query) =
                            Self::get_or_open_query(&input.name, ctx, cluster, db_handles, failed_clusters).await
                        else {
                            warn!("Item not found in Xtream DB for generic probe: {unique_id}");
                            return Ok((false, false));
                        };

                        let query = Arc::clone(&query);
                        let mut item = match spawn_blocking_limited(move || {
                            let mut guard = query.lock();
                            guard.query_zero_copy(&provider_id).ok().flatten()
                        })
                        .await
                        {
                            Ok(Some(item)) => item,
                            Ok(None) => {
                                warn!("Item not found in Xtream DB: {unique_id}");
                                return Ok((false, false));
                            }
                            Err(err) => {
                                return Err(shared::error::TuliproxError::Config(format!(
                                    "Failed to query generic probe item {unique_id}: {err}"
                                )));
                            }
                        };

                        Self::apply_pending_generic_probe_base(collector, cluster, provider_id, &mut item);

                        update_properties(
                            &mut item.additional_properties,
                            *item_type,
                            &item.name,
                            item.virtual_id.get(),
                            metadata.raw_video,
                            metadata.raw_audio,
                            metadata.stats,
                        );

                        match item.additional_properties {
                            Some(StreamProperties::Live(props)) => collector.add_live(provider_id.into(), *props),
                            Some(StreamProperties::Video(props)) => collector.add_vod(provider_id.into(), *props),
                            _ => {}
                        }

                        return Ok((false, false));
                    }
                }

                // Non-Xtream generic probe still writes directly and can target
                // M3U or library storage, so release cached Xtream read handles.
                if !db_handles.is_empty() {
                    db_handles.clear();
                }
                let outcome = update_generic_stream_metadata(
                    &ctx.app_config,
                    client,
                    input.as_ref(),
                    unique_id,
                    url,
                    *item_type,
                    &ctx.active_provider,
                    active_handle,
                    probe_priority,
                    &ctx.events,
                )
                .await?;

                match outcome {
                    GenericProbeOutcome::Updated | GenericProbeOutcome::Noop => Ok((false, false)),
                    GenericProbeOutcome::ProbeFailed => Err(shared::error::TuliproxError::Config(format!(
                        "Probe stream task failed for key {task_key:?} ({probe_identifier})"
                    ))),
                }
            }
        }
    }
}
