use super::{
    acquisition::InputAcquisition, download_input, is_input_enabled, is_target_enabled, join_arc_strs, process_targets,
    target_waiting_message, with_sequential_group, InputCompletionFacts, PlaylistProcessingContext, XmltvEpgProvider,
};
use crate::{fetched_playlist::FetchedPlaylist, metadata_sink::MetadataUpdateSink, parser::xmltv::TVGuide};
use futures::{FutureExt, StreamExt};
use log::{debug, error, log_enabled, warn, Level};
use shared::{
    concat_string,
    error::TuliproxError,
    model::{
        ClusterFlags, EventMessage, EventSink, InputRefreshPolicy, InputStats, InputType, PlaylistStats,
        PlaylistUpdateInputTelemetry, PlaylistUpdateProgressEvent, PlaylistUpdateRunId, PlaylistUpdateRunOrder,
        PlaylistUpdateState, SourceStats, TargetStats,
    },
    utils::sanitize_sensitive_info,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Weak},
    time::Instant,
};
use tokio::{
    sync::{OwnedRwLockWriteGuard, RwLock},
    task::JoinSet,
};
use tuliprox_core::{
    model::{ClusterUpdateRejection, ConfigInput},
    utils::{debug_if_enabled, log_memory_snapshot, StepMeasureCallback},
};
use tuliprox_iptv::epg::{CountingEpgSink, EpgFetchRequest, EpgProvider};
use tuliprox_repository::PlaylistSource;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputJobState {
    Ready,
    Pending,
    Failed,
}

pub(crate) struct InputDownloadResult {
    /// Only a completely read Library catalog (or its persisted input) may be empty.
    pub(crate) authoritative_library: bool,
    pub(crate) errors: Vec<TuliproxError>,
    pub(crate) source: PlaylistSource,
    pub(crate) storage_error: Option<TuliproxError>,
    pub(crate) partial: bool,
    pub(crate) quality_rejections: Vec<ClusterUpdateRejection>,
    pub(crate) accepted_empty_clusters: ClusterFlags,
    pub(crate) input_telemetry: Option<PlaylistUpdateInputTelemetry>,
}

impl InputDownloadResult {
    pub(in crate::processor::playlist) fn update_state(&mut self) -> PlaylistUpdateState {
        InputCompletionFacts {
            job_state: self.job_state(),
            had_errors: !self.errors.is_empty() || self.storage_error.is_some(),
            had_quality_rejections: !self.quality_rejections.is_empty(),
        }
        .update_state()
    }

    pub(crate) fn job_state(&mut self) -> InputJobState {
        if self.partial {
            InputJobState::Pending
        } else if self.storage_error.is_some()
            || (self.source.is_empty() && self.accepted_empty_clusters.is_empty() && !self.authoritative_library)
        {
            InputJobState::Failed
        } else {
            InputJobState::Ready
        }
    }
}

pub(crate) struct InputJobResult {
    pub(crate) index: usize,
    pub(crate) input_id: u16,
    pub(crate) input_name: Arc<str>,
    pub(crate) state: InputJobState,
    pub(crate) source: Option<PlaylistSource>,
    pub(crate) epg: Option<TVGuide>,
    pub(crate) stat: InputStats,
    pub(crate) errors: Vec<TuliproxError>,
    pub(crate) accepted_empty_clusters: ClusterFlags,
    pub(crate) had_quality_rejections: bool,
    pub(crate) input_telemetry: Option<PlaylistUpdateInputTelemetry>,
}

impl InputJobResult {
    pub(crate) fn update_state(&self) -> PlaylistUpdateState {
        InputCompletionFacts {
            job_state: self.state,
            had_errors: !self.errors.is_empty(),
            had_quality_rejections: self.had_quality_rejections,
        }
        .update_state()
    }
}

impl InputCompletionFacts {
    pub(super) fn update_state(self) -> PlaylistUpdateState {
        if self.job_state == InputJobState::Failed || self.had_errors {
            PlaylistUpdateState::Failure
        } else if self.job_state == InputJobState::Pending || self.had_quality_rejections {
            PlaylistUpdateState::Partial
        } else {
            PlaylistUpdateState::Success
        }
    }
}

pub(crate) async fn process_input_job<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    index: usize,
    ctx: &PlaylistProcessingContext<E, M>,
    input: &Arc<ConfigInput>,
    process_parallel: bool,
) -> InputJobResult {
    with_sequential_group(
        &ctx.config.file_locks,
        input.sequential_group,
        process_parallel,
        process_input_job_inner(index, ctx, input),
    )
    .await
}

pub(crate) async fn process_input_job_inner<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    index: usize,
    ctx: &PlaylistProcessingContext<E, M>,
    input: &Arc<ConfigInput>,
) -> InputJobResult {
    let start_time = Instant::now();
    let input_type = input.get_download_input_type();
    let broadcast_step = create_input_broadcast_callback(&ctx.events, &ctx.run_id, ctx.execution_order, input.id);
    broadcast_step("Playlist download", &format!("Downloading input '{}'", input.name));

    let mut download = download_input(ctx, input, false).await;
    let state = download.job_state();
    let storage_failed = download.storage_error.is_some();
    let had_quality_rejections = !download.quality_rejections.is_empty();
    if had_quality_rejections {
        ctx.had_quality_rejections.store(true, std::sync::atomic::Ordering::Release);
    }
    if let Some(err) = download.storage_error.take() {
        broadcast_step("Playlist download", &format!("Failed to persist/load input '{}' playlist", input.name));
        error!("Failed to persist input playlist {}", input.name);
        download.errors.push(err);
    }
    let epg = if input_type == InputType::Library || download.partial || storage_failed {
        None
    } else {
        download_input_epg(ctx, input, &mut download.errors).await
    };
    let group_count = download.source.get_group_count();
    let channel_count = download.source.get_channel_count();
    if state == InputJobState::Failed && download.source.is_empty() && download.accepted_empty_clusters.is_empty() {
        broadcast_step("Playlist download", &format!("Input '{}' playlist is empty", input.name));
        download.errors.push(TuliproxError::RepositoryPlaylist(format!("Source is empty {}", input.name)));
    }
    let stat = create_input_stat(
        group_count,
        channel_count,
        download.errors.len(),
        input_type,
        &input.name,
        start_time.elapsed().as_secs(),
    );

    InputJobResult {
        index,
        input_id: input.id,
        input_name: input.name.clone(),
        state,
        source: (state == InputJobState::Ready).then_some(download.source),
        epg,
        stat,
        errors: download.errors,
        accepted_empty_clusters: download.accepted_empty_clusters,
        had_quality_rejections,
        input_telemetry: download.input_telemetry,
    }
}

pub(crate) fn panicked_input_job(index: usize, input: &ConfigInput) -> InputJobResult {
    let error = TuliproxError::RepositoryPlaylist(format!("Input '{}' processing panicked", input.name));
    InputJobResult {
        index,
        input_id: input.id,
        input_name: input.name.clone(),
        state: InputJobState::Failed,
        source: None,
        epg: None,
        stat: create_input_stat(0, 0, 1, input.get_download_input_type(), &input.name, 0),
        errors: vec![error],
        accepted_empty_clusters: ClusterFlags::empty(),
        had_quality_rejections: false,
        input_telemetry: None,
    }
}

pub(crate) fn report_input_job_completion<E: EventSink>(
    events: &E,
    run_id: &PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
    result: &InputJobResult,
) {
    report_input_completion(
        events,
        run_id,
        execution_order,
        result.input_id,
        &result.input_name,
        result.update_state(),
        result.input_telemetry.as_ref(),
    );
}

pub(in crate::processor::playlist) fn report_input_completion<E: EventSink>(
    events: &E,
    run_id: &PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
    input_id: u16,
    input_name: &str,
    state: PlaylistUpdateState,
    input_telemetry: Option<&PlaylistUpdateInputTelemetry>,
) {
    let input_name = sanitize_sensitive_info(input_name).into_owned();
    let message = match state {
        PlaylistUpdateState::Success => format!("Input '{input_name}' completed successfully"),
        PlaylistUpdateState::Partial => format!("Input '{input_name}' completed partially"),
        PlaylistUpdateState::Failure => format!("Input '{input_name}' failed during update"),
    };
    let progress = PlaylistUpdateProgressEvent::input_completed(
        run_id.clone(),
        execution_order,
        input_id,
        state,
        input_name,
        message,
    );
    let progress = match input_telemetry {
        Some(input_telemetry) => progress.with_input_telemetry(input_telemetry.clone()),
        None => progress,
    };
    events.emit(EventMessage::PlaylistUpdateProgress(progress));
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn process_source<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    source_idx: usize,
    ctx: Arc<PlaylistProcessingContext<E, M>>,
) -> (Vec<InputStats>, Vec<TargetStats>, Vec<TuliproxError>) {
    log_memory_snapshot(format!("source[{source_idx}] start").as_str());
    let sources = ctx.config.sources.load();
    let mut errors = vec![];
    let mut input_stats = HashMap::<Arc<str>, InputStats>::new();
    let mut target_stats = Vec::<TargetStats>::new();
    if let Some(source) = sources.get_source_at(source_idx) {
        let mut source_playlists = Vec::with_capacity(source.inputs.len());
        let broadcast_step = create_broadcast_callback(&ctx.events, &ctx.run_id, ctx.execution_order);
        let process_parallel = ctx.config.config.load().process_parallel;
        let mut disabled_inputs: Vec<Arc<str>> = vec![];
        let mut enabled_inputs = Vec::with_capacity(source.inputs.len());
        for (index, input_name) in source.inputs.iter().enumerate() {
            let Some(input) = sources.get_input_by_name(input_name) else {
                error!("Input {input_name} referenced by source {source_idx} does not exist");
                continue;
            };
            if is_input_enabled(input, &ctx.user_targets) {
                enabled_inputs.push((index, input));
            } else {
                disabled_inputs.push(input.name.clone());
            }
        }

        let source_downloaded = !enabled_inputs.is_empty();
        let mut job_results = Vec::with_capacity(enabled_inputs.len());
        if process_parallel {
            let mut jobs = futures::stream::FuturesUnordered::new();
            for &(index, input) in &enabled_inputs {
                let job = std::panic::AssertUnwindSafe(process_input_job(index, &ctx, input, true)).catch_unwind();
                jobs.push(async move {
                    match job.await {
                        Ok(result) => result,
                        Err(_) => panicked_input_job(index, input),
                    }
                });
            }
            while let Some(result) = jobs.next().await {
                job_results.push(result);
            }
        } else {
            for &(index, input) in &enabled_inputs {
                job_results.push(process_input_job(index, &ctx, input, false).await);
            }
        }
        job_results.sort_by_key(|result| result.index);

        let mut blockers = Vec::new();
        let mut accepted_empty_clusters = ClusterFlags::empty();
        for mut result in job_results {
            super::super::input_status::persist_input_job_result(&ctx, &result).await;
            report_input_job_completion(&ctx.events, &ctx.run_id, ctx.execution_order, &result);
            errors.append(&mut result.errors);
            input_stats.insert(result.input_name.clone(), result.stat);
            if result.state == InputJobState::Ready {
                accepted_empty_clusters |= result.accepted_empty_clusters;
                if let (Some(input), Some(source)) =
                    (sources.get_input_by_name(&result.input_name), result.source.take())
                {
                    source_playlists.push(FetchedPlaylist { input, source, epg: result.epg });
                }
            } else {
                blockers.push(result.input_name);
            }
        }

        if !disabled_inputs.is_empty() && !source_downloaded {
            warn!(
                "Source at index {source_idx} has no enabled inputs for the given targets. Disabled: {}",
                join_arc_strs(&disabled_inputs, ", ")
            );
        }
        if source_downloaded {
            if !blockers.is_empty() {
                for target in source.targets.iter().filter(|target| is_target_enabled(target, &ctx.user_targets)) {
                    for input_name in &blockers {
                        broadcast_step("Playlist download", &target_waiting_message(&target.name, input_name));
                    }
                }
            } else if source_playlists.is_empty() {
                debug!("Source at index {source_idx} is empty");
                errors.push(TuliproxError::RepositoryPlaylist(format!(
                    "Source at index {source_idx} is empty: {}",
                    join_arc_strs(&source.inputs, ", ")
                )));
            } else {
                debug_if_enabled!(
                    "Source has {} groups",
                    source_playlists.iter_mut().map(FetchedPlaylist::get_channel_count).sum::<usize>()
                );
                let enabled_targets: Vec<_> =
                    source.targets.iter().filter(|target| is_target_enabled(target, &ctx.user_targets)).collect();
                target_stats = process_targets(
                    &ctx,
                    &mut source_playlists,
                    &enabled_targets,
                    &mut input_stats,
                    &mut errors,
                    accepted_empty_clusters,
                    process_parallel,
                )
                .await;
            }
        }
    }
    log_memory_snapshot(format!("source[{source_idx}] end").as_str());
    let ordered_input_stats = sources
        .get_source_at(source_idx)
        .map_or_else(Vec::new, |source| source.inputs.iter().filter_map(|name| input_stats.remove(name)).collect());
    (ordered_input_stats, target_stats, errors)
}

pub(crate) async fn download_input_epg<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    input: &Arc<ConfigInput>,
    error_list: &mut Vec<TuliproxError>,
) -> Option<TVGuide> {
    // A failed playlist download makes the EPG moot: the channels it would annotate are
    // not there.
    if !error_list.is_empty() {
        return None;
    }
    let provider = XmltvEpgProvider::new(ctx);
    // The XMLTV path produces documents, not programme records, so nothing reaches the
    // sink. It is here because the same call answers for a record-streaming provider.
    let mut discarded = CountingEpgSink::new();
    let outcome = provider.fetch(&EpgFetchRequest::new(input), &mut discarded).await;
    error_list.extend(provider.take_errors());
    match outcome {
        Ok(outcome) => outcome.into_guide(),
        Err(err) => {
            error_list.push(err);
            None
        }
    }
}

pub(crate) fn create_broadcast_callback<E: EventSink + Clone + 'static>(
    events: &E,
    run_id: &PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
) -> StepMeasureCallback {
    let events = events.clone();
    let run_id = run_id.clone();
    Box::new(move |context: &str, msg: &str| {
        events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::for_run_global(
            run_id.clone(),
            execution_order,
            context,
            msg,
        )));
    })
}

fn create_input_broadcast_callback<E: EventSink + Clone + 'static>(
    events: &E,
    run_id: &PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
    input_id: u16,
) -> StepMeasureCallback {
    let events = events.clone();
    let run_id = run_id.clone();
    Box::new(move |context: &str, msg: &str| {
        events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::for_run_input(
            run_id.clone(),
            execution_order,
            input_id,
            context,
            msg,
        )));
    })
}

pub(crate) fn create_input_stat(
    group_count: usize,
    channel_count: usize,
    error_count: usize,
    input_type: InputType,
    input_name: &str,
    secs_took: u64,
) -> InputStats {
    InputStats {
        name: input_name.to_string(),
        input_type,
        error_count,
        raw_stats: PlaylistStats { group_count, channel_count },
        processed_stats: PlaylistStats { group_count: 0, channel_count: 0 },
        secs_took,
    }
}

// Written out rather than derived: `#[derive(Clone)]` would demand `M: Clone`,
// but the sink is held behind an `Arc` and is cloneable whatever `M` is.
impl<E: EventSink + Clone, M: MetadataUpdateSink> Clone for PlaylistProcessingContext<E, M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            run_id: self.run_id.clone(),
            execution_order: self.execution_order,
            config: Arc::clone(&self.config),
            user_targets: Arc::clone(&self.user_targets),
            events: self.events.clone(),
            playlist_state: self.playlist_state.clone(),
            disabled_headers: self.disabled_headers.clone(),
            processed_inputs: Arc::clone(&self.processed_inputs),
            input_completions: Arc::clone(&self.input_completions),
            input_locks: Arc::clone(&self.input_locks),
            provider_manager: self.provider_manager.clone(),
            metadata_manager: self.metadata_manager.clone(),
            pre_processed_inputs: self.pre_processed_inputs.clone(),
            stalker_refresh_mode: self.stalker_refresh_mode,
            partial_refresh: Arc::clone(&self.partial_refresh),
            had_quality_rejections: Arc::clone(&self.had_quality_rejections),
            input_refresh: self.input_refresh,
            library_update_mode: self.library_update_mode,
        }
    }
}

impl<E: EventSink + Clone + 'static, M: MetadataUpdateSink> PlaylistProcessingContext<E, M> {
    #[must_use]
    pub fn refresh_policy(&self, input_id: u16) -> InputRefreshPolicy {
        self.input_refresh
            .filter(|input_refresh| input_refresh.input_id == input_id)
            .map_or(InputRefreshPolicy::NORMAL, |input_refresh| input_refresh.policy)
    }

    pub(super) fn acquisition(&self, input_id: u16) -> InputAcquisition {
        if self.library_update_mode.reloads_input(input_id) {
            InputAcquisition::RescannedLibrary
        } else {
            InputAcquisition::Provider(self.refresh_policy(input_id))
        }
    }

    pub async fn is_input_downloaded(&self, input_name: &str) -> bool {
        let processed = self.processed_inputs.lock().await;
        processed.contains(input_name)
    }
    pub async fn mark_input_downloaded(&self, input_name: Arc<str>) -> bool {
        let mut processed = self.processed_inputs.lock().await;
        processed.insert(input_name)
    }

    pub async fn get_input_lock(&self, input_name: &Arc<str>) -> OwnedRwLockWriteGuard<()> {
        let mut locks = self.input_locks.lock().await;
        // Try to upgrade the existing weak reference
        let lock = locks.get(input_name).and_then(Weak::upgrade).unwrap_or_else(|| {
            let new_lock = Arc::new(RwLock::new(()));
            locks.insert(input_name.clone(), Arc::downgrade(&new_lock));
            new_lock
        });

        // Clean up stale references periodically
        locks.retain(|_, weak| weak.strong_count() > 0);

        drop(locks); // Release mutex before awaiting write lock
        lock.write_owned().await
    }
}

pub(crate) async fn process_sources<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    processing_ctx: &PlaylistProcessingContext<E, M>,
) -> (Vec<SourceStats>, Vec<TuliproxError>) {
    let mut async_tasks = JoinSet::new();
    let sources = processing_ctx.config.sources.load();
    let process_parallel = processing_ctx.config.config.load().process_parallel;
    if process_parallel && log_enabled!(Level::Debug) {
        debug!("Parallel processing enabled");
    }

    let mut source_results = Vec::new();
    let mut errors = Vec::new();
    let mut processed_any = false;

    for (index, source) in sources.sources.iter().enumerate() {
        if !source.should_process_for_user_targets(&processing_ctx.user_targets) {
            continue;
        }

        // We're using the file lock this way on purpose
        let source_lock_path = PathBuf::from(concat_string!("source_", &index.to_string()));
        let Ok(update_lock) = processing_ctx.config.file_locks.try_write_lock(&source_lock_path).await else {
            warn!(
                "The update operation for the source at index {index} was skipped because an update is already in progress."
            );
            continue;
        };

        let ctx = Arc::new(processing_ctx.clone());

        processed_any = true;
        if process_parallel {
            async_tasks.spawn(async move {
                let _update_lock = update_lock;
                (index, process_source(index, ctx).await)
            });
        } else {
            source_results.push((index, process_source(index, ctx).await));
            drop(update_lock);
        }
    }
    if !processed_any {
        warn!(
            "No sources were processed for the given targets. Check that:\n\
             - Sources have enabled targets matching your target selection\n\
             - CLI -t filter or schedule.targets are correct\n\
             - No playlist lock is blocking updates"
        );
    }
    while let Some(result) = async_tasks.join_next().await {
        match result {
            Ok(result) => source_results.push(result),
            Err(err) => {
                error!("Playlist processing task failed: {err:?}");
                errors
                    .push(TuliproxError::RepositoryPlaylist(format!("Playlist source processing task failed: {err}")));
            }
        }
    }

    source_results.sort_by_key(|(index, _)| *index);
    let mut stats = Vec::with_capacity(source_results.len());
    for (_, (input_stats, target_stats, mut source_errors)) in source_results {
        errors.append(&mut source_errors);
        if let Some(source_stats) = SourceStats::try_new(input_stats, target_stats) {
            stats.push(source_stats);
        }
    }
    (stats, errors)
}
