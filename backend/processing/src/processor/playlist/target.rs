#![allow(clippy::wildcard_imports)]
use super::*;

pub(crate) fn join_arc_strs(values: &[Arc<str>], separator: &str) -> String {
    let mut result = String::new();
    for value in values {
        if !result.is_empty() {
            result.push_str(separator);
        }
        result.push_str(value.as_ref());
    }
    result
}

pub(crate) fn target_waiting_message(target: &str, input: &str) -> String {
    format!("Target '{target}' is waiting for input '{input}'")
}

pub(crate) fn target_mutated_resources(
    config: &tuliprox_core::model::Config,
    target: &ConfigTarget,
) -> HashSet<PathBuf> {
    let mut resources = HashSet::new();
    if let Some(path) = tuliprox_repository::get_target_storage_path(config, &target.name) {
        resources.insert(path.clean());
    }
    for output in &target.output {
        match output {
            tuliprox_core::model::TargetOutput::M3u(output) => {
                if let Some(path) = tuliprox_core::utils::get_file_path(
                    &config.storage_dir,
                    output.filename.as_deref().map(PathBuf::from),
                ) {
                    resources.insert(path.clean());
                }
            }
            tuliprox_core::model::TargetOutput::Strm(output) => {
                if let Some(path) =
                    tuliprox_core::utils::get_file_path(&config.storage_dir, Some(PathBuf::from(&output.directory)))
                {
                    resources.insert(path.clean());
                }
            }
            tuliprox_core::model::TargetOutput::Xtream(_) | tuliprox_core::model::TargetOutput::HdHomeRun(_) => {}
        }
    }
    resources
}

pub(crate) fn stalker_checkpoint_message(input: &str) -> String {
    format!("Input '{input}': Stalker refresh checkpoint saved; active snapshot remains in service")
}

pub(crate) fn is_target_enabled(target: &ConfigTarget, user_targets: &ProcessTargets) -> bool {
    (!user_targets.enabled && target.enabled) || (user_targets.enabled && user_targets.has_target(target.id))
}

pub(crate) struct TargetJobResult {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) result: Result<(), Vec<TuliproxError>>,
    pub(crate) errors: Vec<TuliproxError>,
    pub(crate) processing: PipelineStats,
}

pub(crate) fn collect_target_task_result(
    result: Result<TargetJobResult, tokio::task::JoinError>,
    results: &mut Vec<TargetJobResult>,
    errors: &mut Vec<TuliproxError>,
) {
    match result {
        Ok(result) => results.push(result),
        Err(err) => errors.push(TuliproxError::RepositoryPlaylist(format!("Target finalization task failed: {err}"))),
    }
}

pub(crate) async fn wait_for_target_finalizer_slot(
    tasks: &mut JoinSet<TargetJobResult>,
    results: &mut Vec<TargetJobResult>,
    errors: &mut Vec<TuliproxError>,
) {
    if tasks.len() >= MAX_CONCURRENT_TARGET_FINALIZERS {
        if let Some(result) = tasks.join_next().await {
            collect_target_task_result(result, results, errors);
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn process_targets<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &Arc<PlaylistProcessingContext<E, M>>,
    playlists: &mut [FetchedPlaylist<'_>],
    targets: &[&Arc<ConfigTarget>],
    input_stats: &mut HashMap<Arc<str>, InputStats>,
    errors: &mut Vec<TuliproxError>,
    process_parallel: bool,
) -> Vec<TargetStats> {
    if !process_parallel {
        let mut target_stats = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let consume_input_source = index + 1 == targets.len();
            let result =
                prepare_playlist_for_target(ctx, playlists, target, input_stats, errors, consume_input_source).await;
            match result {
                Ok(prepared) => {
                    let processing = prepared.processing.clone();
                    let (result, mut finalization_errors) = finalize_prepared_target(Arc::clone(ctx), prepared).await;
                    errors.append(&mut finalization_errors);
                    match result {
                        Ok(()) => target_stats.push(TargetStats::success_with_processing(&target.name, processing)),
                        Err(mut target_errors) => {
                            target_stats.push(TargetStats::failure_with_processing(&target.name, processing));
                            errors.append(&mut target_errors);
                        }
                    }
                }
                Err(mut target_errors) => {
                    target_stats.push(TargetStats::failure(&target.name));
                    errors.append(&mut target_errors);
                }
            }
        }
        return target_stats;
    }

    let resources = {
        let config = ctx.config.config.load();
        targets.iter().map(|target| target_mutated_resources(&config, target)).collect::<Vec<_>>()
    };
    let mut completion_receivers: Vec<watch::Receiver<bool>> = Vec::with_capacity(targets.len());
    let mut tasks = JoinSet::new();
    let mut results = Vec::with_capacity(targets.len());

    for (index, target) in targets.iter().enumerate() {
        wait_for_target_finalizer_slot(&mut tasks, &mut results, errors).await;
        let predecessors = resources[..index]
            .iter()
            .zip(&completion_receivers)
            .filter(|(earlier, _)| !earlier.is_disjoint(&resources[index]))
            .map(|(_, receiver)| receiver.clone())
            .collect::<Vec<_>>();
        let (completion, receiver) = watch::channel(false);
        completion_receivers.push(receiver);

        match prepare_playlist_for_target(ctx, playlists, target, input_stats, errors, false).await {
            Ok(prepared) => {
                let processing = prepared.processing.clone();
                let task_ctx = Arc::clone(ctx);
                let target_name = target.name.clone();
                tasks.spawn(async move {
                    for mut predecessor in predecessors {
                        if !*predecessor.borrow() {
                            let _ = predecessor.changed().await;
                        }
                    }
                    let finalized =
                        std::panic::AssertUnwindSafe(finalize_prepared_target(task_ctx, prepared)).catch_unwind().await;
                    completion.send_replace(true);
                    match finalized {
                        Ok((result, errors)) => {
                            TargetJobResult { index, name: target_name, result, errors, processing }
                        }
                        Err(_) => TargetJobResult {
                            index,
                            name: target_name.clone(),
                            result: Err(vec![TuliproxError::RepositoryPlaylist(format!(
                                "Target '{target_name}' finalization panicked"
                            ))]),
                            errors: Vec::new(),
                            processing,
                        },
                    }
                });
            }
            Err(target_errors) => {
                completion.send_replace(true);
                results.push(TargetJobResult {
                    index,
                    name: target.name.clone(),
                    result: Err(target_errors),
                    errors: Vec::new(),
                    processing: PipelineStats::default(),
                });
            }
        }
    }

    while let Some(result) = tasks.join_next().await {
        collect_target_task_result(result, &mut results, errors);
    }
    results.sort_by_key(|result| result.index);

    let mut target_stats = Vec::with_capacity(results.len());
    for mut target_result in results {
        errors.append(&mut target_result.errors);
        match target_result.result {
            Ok(()) => {
                target_stats.push(TargetStats::success_with_processing(&target_result.name, target_result.processing));
            }
            Err(mut target_errors) => {
                target_stats.push(TargetStats::failure_with_processing(&target_result.name, target_result.processing));
                errors.append(&mut target_errors);
            }
        }
    }
    target_stats
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalizationStage {
    Merge,
    Deduplicate,
    Sort,
    AssignChannelNumbers,
    AssignCounters,
}

pub(crate) const FINALIZATION_ORDER: [FinalizationStage; 5] = [
    FinalizationStage::Merge,
    FinalizationStage::Deduplicate,
    FinalizationStage::Sort,
    FinalizationStage::AssignChannelNumbers,
    FinalizationStage::AssignCounters,
];

pub(crate) fn apply_persist_filter(target: &ConfigTarget, groups: &mut Vec<PlaylistGroup>) {
    let Some(filter) = target.filter.persist.as_ref() else {
        return;
    };
    let outcome = retain_filtered_playlist(groups, filter);
    debug!("Target '{}' persist filter outcome: {outcome:?}", target.name);
}

pub(crate) struct PreparedTarget {
    pub(crate) target: ConfigTarget,
    pub(crate) playlist: Vec<PlaylistGroup>,
    pub(crate) epg: Vec<Epg>,
    pub(crate) processing: PipelineStats,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_playlist_for_target<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    playlists: &mut [FetchedPlaylist<'_>],
    target: &ConfigTarget,
    stats: &mut HashMap<Arc<str>, InputStats>,
    errors: &mut Vec<TuliproxError>,
    consume_input_source: bool,
) -> Result<PreparedTarget, Vec<TuliproxError>> {
    debug_if_enabled!("Processing order is {}", &target.processing_order);
    log_memory_snapshot(format!("target '{}' start", target.name).as_str());

    let mut duplicates: HashSet<UUIDType> = HashSet::new();
    let mut new_epg = vec![];
    let mut new_playlist: Vec<PlaylistGroup> = vec![];
    let mut aggregate_outcome = PipelineOutcome::default();

    debug!("Executing processing pipes");
    let broadcast_step = create_broadcast_callback(&ctx.events);

    let bouquet_file =
        tuliprox_repository::load_target_bouquet(&ctx.config, &target.name).await.map_err(|err| vec![err])?;
    let bouquet_filter =
        bouquet_file.and_then(|file| tuliprox_core::model::TargetBouquetFilter::from_dto(file.bouquet));
    if let Some(ref filter) = bouquet_filter {
        let (live, vod, series) = filter.cluster_counts();
        debug!("Loaded target bouquet for '{}': live={:?}, vod={:?}, series={:?}", target.name, live, vod, series);
    }

    let pipe = get_processing_pipe(target);
    let mut step = StepMeasure::new(&target.name, broadcast_step);
    for provider_fpl in playlists.iter_mut() {
        log_memory_snapshot(
            format!("target '{}' input '{}' before_pipe", target.name, provider_fpl.input.name).as_str(),
        );
        step.broadcast("Executing transformations on '{}' playlist", &target.name);
        let (mut processed_fpl, input_outcome) =
            execute_pipe(target, &pipe, provider_fpl, &mut duplicates, consume_input_source, bouquet_filter.as_ref())
                .map_err(|err| vec![err])?;
        debug!("Target '{}' input '{}' pipeline outcome: {input_outcome:?}", target.name, provider_fpl.input.name);
        aggregate_outcome.merge(input_outcome);
        log_memory_snapshot(
            format!("target '{}' input '{}' after_pipe", target.name, provider_fpl.input.name).as_str(),
        );
        processed_fpl.sort_by_provider_ordinal();
        playlist_resolve(ctx, target, errors, &pipe, provider_fpl, &mut processed_fpl).await;
        log_memory_snapshot(
            format!("target '{}' input '{}' after_vod_resolve", target.name, provider_fpl.input.name).as_str(),
        );
        let clear_invalid_epg_ids = target.options.as_ref().is_some_and(ConfigTargetOptions::clear_invalid_epg_ids);
        let input_epg_start = new_epg.len();
        process_playlist_epg(&mut processed_fpl, &mut new_epg, clear_invalid_epg_ids).await;
        log_memory_snapshot(
            format!("target '{}' input '{}' after_epg_apply", target.name, processed_fpl.input.name).as_str(),
        );
        let deduplicate = target.execution_plan.pre_transform_identity_dedup;
        if let Some(groups) = map_playlist_at_stage(
            &mut processed_fpl.source,
            target,
            MappingStage::AfterEpg,
            deduplicate.then_some(&mut duplicates),
        ) {
            processed_fpl.source = MemoryPlaylistSource::new(groups).into_source();
        }
        if clear_invalid_epg_ids && processed_fpl.epg.is_some() {
            clear_invalid_live_epg_ids(&mut processed_fpl, &new_epg[input_epg_start..]);
        }
        if let Some(stat) = stats.get_mut(&processed_fpl.input.name) {
            stat.processed_stats.group_count = processed_fpl.get_group_count();
            stat.processed_stats.channel_count = processed_fpl.get_channel_count();
        }
        new_playlist.extend(processed_fpl.source.take_groups());
        log_memory_snapshot(
            format!("target '{}' input '{}' after_take_groups", target.name, processed_fpl.input.name).as_str(),
        );
        tokio::task::yield_now().await;
    }
    step.tick("filter rename map + epg");
    log_memory_snapshot(format!("target '{}' after_filter_rename_map_epg", target.name).as_str());
    step.stop("Preparing playlist");
    Ok(PreparedTarget {
        target: target.clone(),
        playlist: new_playlist,
        epg: new_epg,
        processing: aggregate_outcome.to_stats(),
    })
}

/// Spill each `Epg` source to a temp `BPlusTree` and merge them. Extracted
/// from `finalize_prepared_target` so it can be unit-tested without
/// constructing a full `PlaylistProcessingContext`.
///
/// Returns `Ok(None)` if `sources` is empty (no EPG to merge), matching
/// the contract of `flatten_tvguide`. The temp directory lives inside
/// this function call — all temp files are removed by the
/// `DiskEpgSource` drop guards before this function returns.
pub(crate) fn spill_epg_to_disk(sources: Vec<Epg>) -> Result<Option<Epg>, TuliproxError> {
    let dir =
        tempfile::tempdir().map_err(|e| TuliproxError::RepositoryXtream(format!("tempdir for EPG spill: {e}")))?;
    let mut disk_sources = Vec::with_capacity(sources.len());
    for (source_order, guide) in sources.into_iter().enumerate() {
        let mut acc = EpgMergeAccumulator::new();
        acc.set_attributes_if_preferred(guide.priority, source_order, guide.attributes);
        for channel in guide.children {
            acc.add_channel_with_programmes(
                guide.priority,
                source_order,
                guide.logo_override,
                std::sync::Arc::unwrap_or_clone(channel),
            );
        }
        let path = dir.path().join(format!("epg-src-{source_order}.db"));
        let source_order_u32 = u32::try_from(source_order).unwrap_or(0);
        let source = acc
            .finish_into_disk(path, guide.priority, source_order_u32)
            .map_err(|e| TuliproxError::RepositoryXtream(format!("EPG spill to disk failed: {e}")))?;
        disk_sources.push(source);
    }
    if disk_sources.is_empty() {
        Ok(None)
    } else {
        merge_epg_trees(disk_sources)
            .map_err(|e| TuliproxError::RepositoryXtream(format!("EPG disk merge failed: {e}")))
            .map(|opt| opt.map(|(epg, _)| epg))
    }
}

pub(crate) async fn finalize_prepared_target<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: Arc<PlaylistProcessingContext<E, M>>,
    prepared: PreparedTarget,
) -> (Result<(), Vec<TuliproxError>>, Vec<TuliproxError>) {
    let target = &prepared.target;
    let mut new_playlist = prepared.playlist;
    let mut new_epg = prepared.epg;
    let mut errors = Vec::new();
    let broadcast_step = create_broadcast_callback(&ctx.events);
    let mut step = StepMeasure::new(&target.name, broadcast_step);
    if target.favourites.is_some() {
        step.broadcast("Processing favourites for '{}' playlist", &target.name);
        process_favourites(&mut new_playlist, target.favourites.as_deref());
        log_memory_snapshot(format!("target '{}' after_favourites", target.name).as_str());
    }

    if new_playlist.is_empty() {
        step.stop("");
        info!("Playlist is empty: {}", target.name);
        (Ok(()), errors)
    } else {
        // Process Trakt categories
        if trakt_playlist(&ctx.client, target, &mut errors, &mut new_playlist).await {
            step.tick("trakt categories");
            log_memory_snapshot(format!("target '{}' after_trakt", target.name).as_str());
        }

        let mut flat_new_playlist = flatten_groups(new_playlist);
        step.tick("playlist merge");
        log_memory_snapshot(format!("target '{}' after_playlist_merge", target.name).as_str());

        for stage in FINALIZATION_ORDER.into_iter().skip(1) {
            match stage {
                FinalizationStage::Merge => unreachable!("merge is completed before post-merge finalization"),
                FinalizationStage::Deduplicate => {
                    if let Some(dedup_config) = target.execution_plan.post_merge_content_dedup.as_ref() {
                        let removed =
                            crate::processor::deduplicate::deduplicate_playlist(*dedup_config, &mut flat_new_playlist);
                        if removed > 0 {
                            info!("Deduplicated {removed} channels for target {}", target.name);
                        }
                        step.tick("playlist dedup");
                        log_memory_snapshot(format!("target '{}' after_playlist_dedup", target.name).as_str());
                    }
                }
                FinalizationStage::Sort => {
                    if sort_playlist(target, &mut flat_new_playlist) {
                        step.tick("playlist sort");
                        log_memory_snapshot(format!("target '{}' after_playlist_sort", target.name).as_str());
                    }
                }
                FinalizationStage::AssignChannelNumbers => {
                    assign_channel_no_playlist(&mut flat_new_playlist);
                    step.tick("assigning channel numbers");
                    log_memory_snapshot(format!("target '{}' after_assign_channel_numbers", target.name).as_str());
                }
                FinalizationStage::AssignCounters => {
                    map_playlist_counter(target, &mut flat_new_playlist);
                    step.tick("assigning channel counter");
                    log_memory_snapshot(format!("target '{}' after_assign_channel_counter", target.name).as_str());
                }
            }
        }

        apply_persist_filter(target, &mut flat_new_playlist);
        retain_epg_referenced_by_groups(&flat_new_playlist, &mut new_epg);

        let merged_epg = if ctx.config.config.load().disk_based_processing {
            // Per-source drain to disk, then multi-way merge. Errors are pushed
            // to `errors` rather than `?` because the function returns
            // `(Result, Vec<TuliproxError>)`, not `Result` directly. We must
            // surface tempdir / write / merge failures — the user opted in to
            // disk spilling, and silently falling back to the in-memory path
            // can OOM on large feeds. When the spill itself fails we skip the
            // persist step entirely: continuing with `merged_epg = None` would
            // overwrite the existing on-disk EPG with nothing and discard the
            // previously persisted artifact on a transient error.
            match spill_epg_to_disk(new_epg) {
                Ok(epg) => epg,
                Err(err) => {
                    let result_error = TuliproxError::new(err.kind(), err.message());
                    errors.push(err);
                    step.stop("EPG spill failed; skipping persist to preserve existing EPG");
                    log_memory_snapshot(format!("target '{}' after_persist", target.name).as_str());
                    return (Err(vec![result_error]), errors);
                }
            }
        } else {
            flatten_tvguide(new_epg)
        };
        let result = persist_playlist(
            &ctx.config,
            &mut flat_new_playlist,
            merged_epg.as_ref(),
            target,
            ctx.playlist_state.as_ref(),
        )
        .await;
        if result.is_ok() && process_watch(&ctx.config, &ctx.events, target, &flat_new_playlist).await {
            step.tick("group watches");
            log_memory_snapshot(format!("target '{}' after_group_watches", target.name).as_str());
        }
        step.stop("Persisting playlists");
        log_memory_snapshot(format!("target '{}' after_persist", target.name).as_str());
        (result, errors)
    }
}

pub(crate) async fn playlist_resolve<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    pipe: &ProcessingPipe,
    provider_fpl: &mut FetchedPlaylist<'_>,
    processed_fpl: &mut FetchedPlaylist<'_>,
) {
    playlist_resolve_series(ctx, target, errors, pipe, provider_fpl, processed_fpl).await;
    playlist_resolve_vod(ctx, target, errors, provider_fpl, processed_fpl).await;
    playlist_probe(ctx, target, processed_fpl).await;
}

pub(crate) fn is_probe_supported_item_type(item_type: PlaylistItemType) -> bool {
    matches!(
        item_type,
        PlaylistItemType::Live // we skip other live streams because hls and dash have multiple resolutions
                | PlaylistItemType::Video
                | PlaylistItemType::LocalVideo
                | PlaylistItemType::Series
                | PlaylistItemType::LocalSeries
    )
}

pub(crate) fn has_probe_details(item: &PlaylistItem) -> bool {
    match item.header.additional_properties.as_ref() {
        Some(StreamProperties::Video(v)) => v.details.as_ref().is_some_and(|d| d.video.is_some() && d.audio.is_some()),
        Some(StreamProperties::Live(l)) => l.video.is_some() && l.audio.is_some() && l.bitrate > 0,
        Some(StreamProperties::Episode(e)) => e.video.is_some() && e.audio.is_some(),
        Some(StreamProperties::Series(_)) | None => false,
    }
}

pub(crate) fn get_live_probe_interval_settings(
    target: &ConfigTarget,
    input_type: InputType,
    input_options: Option<&ConfigInputOptions>,
) -> Option<(u16, u64)> {
    if !(input_type.is_xtream() || input_type.is_m3u() || input_type.is_stalker()) {
        return None;
    }
    target.get_xtream_output().map(|_| {
        let (probe_delay, input_probe_live_interval_hours) = input_options
            .map_or((default_probe_delay_secs(), default_probe_live_interval()), |options| {
                (options.probe_delay, options.probe_live_interval_hours)
            });
        (probe_delay, u64::from(input_probe_live_interval_hours) * 3600)
    })
}

pub(crate) fn needs_live_probe(item: &PlaylistItem, cutoff_ts: i64) -> bool {
    match item.header.additional_properties.as_ref() {
        Some(StreamProperties::Live(props)) => {
            props.bitrate == 0 || props.last_probed_timestamp.is_none_or(|last_ts| last_ts < cutoff_ts)
        }
        _ => true,
    }
}

pub(crate) fn provider_id_from_item(item: &PlaylistItem) -> Option<ProviderIdType> {
    if let Ok(id) = item.header.id.parse::<u32>() {
        if id == 0 {
            return None;
        }
        return Some(ProviderIdType::Id(id));
    }

    let raw = item.header.id.trim();
    if raw.is_empty() {
        None
    } else {
        Some(ProviderIdType::from(raw))
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn playlist_probe<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    target: &ConfigTarget,
    fpl: &mut FetchedPlaylist<'_>,
) {
    let Some(mgr) = ctx.metadata_manager.as_ref() else {
        return;
    };
    let Some(opts) = fpl.input.options.as_ref() else {
        return;
    };
    let probe_live_enabled = opts.has_flag(ConfigInputFlags::ProbeLive);
    let probe_vod_enabled = opts.has_flag(ConfigInputFlags::ProbeVod);
    let probe_series_enabled = opts.has_flag(ConfigInputFlags::ProbeSeries);

    if !(probe_live_enabled || probe_vod_enabled || probe_series_enabled) {
        return;
    }
    if !ctx.config.is_ffprobe_enabled().await {
        return;
    }

    let input_name = fpl.input.name.clone();
    // The first `should_skip_enqueue` for an input needs its persisted enqueue
    // state on disk; inputs where no item reaches that check must not pay for
    // the load, so it happens on first use rather than here.
    let mut enqueue_state_prepared = false;
    let effective_input_type = fpl.input.get_download_input_type();
    let xtream_probe_handled = effective_input_type.is_xtream() && target.get_xtream_output().is_some();
    let live_probe_settings = if probe_live_enabled {
        get_live_probe_interval_settings(target, effective_input_type, Some(opts)).map(|(delay, interval_secs)| {
            let interval_signed = i64::try_from(interval_secs).unwrap_or(i64::MAX);
            let cutoff_ts = chrono::Utc::now().timestamp().saturating_sub(interval_signed);
            (delay, interval_secs, cutoff_ts)
        })
    } else {
        None
    };

    let mut queued_probe_keys: HashSet<(Arc<str>, String)> = HashSet::new();
    let mut queued_live_keys: HashSet<ProviderIdType> = HashSet::new();
    let mut queued_live_count = 0usize;
    let mut queued_stream_count = 0usize;

    let probe_filter = fpl.input.options.as_ref().and_then(|o| o.probe_filter.as_ref());

    for item in fpl.items() {
        if !is_probe_supported_item_type(item.header.item_type) {
            continue;
        }
        match item.header.item_type {
            PlaylistItemType::Live => {
                if !probe_live_enabled {
                    continue;
                }
            }
            PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
                if !probe_vod_enabled {
                    continue;
                }
            }
            PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                if !probe_series_enabled {
                    continue;
                }
            }
            _ => continue,
        }

        // If input has a probe filter and this item doesn't match, skip probing
        if let Some(p_filter) = probe_filter {
            let provider = ValueProvider { pli: &item, match_as_ascii: false };
            if !p_filter.filter(&provider) {
                continue;
            }
        }

        match item.header.item_type {
            PlaylistItemType::Live => {
                if let Some((probe_delay, interval_secs, cutoff_ts)) = live_probe_settings {
                    if needs_live_probe(&item, cutoff_ts) {
                        if let Some(provider_id) = provider_id_from_item(&item) {
                            if queued_live_keys.insert(provider_id.clone()) {
                                let task = UpdateTask::ProbeLive {
                                    id: provider_id.clone(),
                                    reason: ResolveReason::Probe.into(),
                                    delay: probe_delay,
                                    interval: interval_secs,
                                };
                                if !enqueue_state_prepared {
                                    mgr.prepare_enqueue_state(input_name.clone()).await;
                                    enqueue_state_prepared = true;
                                }
                                if mgr.should_skip_enqueue(&input_name, &task) {
                                    continue;
                                }
                                if log_enabled!(Level::Debug) {
                                    let last_probed = match item.header.additional_properties.as_ref() {
                                        Some(StreamProperties::Live(props)) => props.last_probed_timestamp,
                                        _ => None,
                                    };
                                    debug!(
                                        "[Task] Creating ProbeLive task for input {}: id={}, last_probed_ts={:?}, cutoff_ts={}, interval={}s, title=\"{}\"",
                                        input_name,
                                        provider_id,
                                        last_probed,
                                        cutoff_ts,
                                        interval_secs,
                                        item.header.title
                                    );
                                }
                                Arc::clone(mgr).queue_task_background(input_name.clone(), task);
                                queued_live_count += 1;
                            }
                        }
                    }
                    continue;
                }
                // If live probes are enabled but no live-specific settings are available, fall through to the
                // generic probe path to keep behaviour consistent with non-xtream outputs.
            }
            PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
                // Xtream outputs handle VOD probe as part of the resolve pipeline (after resolve).
                if xtream_probe_handled {
                    continue;
                }
            }
            PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                // Xtream outputs handle Series probe as part of the resolve pipeline (after resolve).
                if xtream_probe_handled {
                    continue;
                }
            }
            _ => continue,
        }

        if has_probe_details(&item) {
            continue;
        }

        // For M3U, ID is a provider id; for Library, ID is UUID.
        let unique_id = if effective_input_type == InputType::Library {
            item.header.uuid.to_valid_uuid()
        } else {
            item.header.id.to_string()
        };
        let probe_scope =
            if item.header.input_name.is_empty() { input_name.clone() } else { item.header.input_name.clone() };

        if !queued_probe_keys.insert((probe_scope.clone(), unique_id.clone())) {
            continue;
        }

        let task = UpdateTask::ProbeStream {
            probe_scope: probe_scope.clone(),
            unique_id: unique_id.clone(),
            url: item.header.url.to_string(),
            item_type: item.header.item_type,
            reason: ResolveReason::MissingDetails.into(),
            delay: opts.probe_delay,
        };
        if !enqueue_state_prepared {
            mgr.prepare_enqueue_state(input_name.clone()).await;
            enqueue_state_prepared = true;
        }
        if mgr.should_skip_enqueue(&input_name, &task) {
            continue;
        }
        debug!(
            "[Task] Creating ProbeStream task for input {}: scope={}, unique_id={}, item_type={:?}, title=\"{}\"",
            input_name, probe_scope, unique_id, item.header.item_type, item.header.title
        );
        Arc::clone(mgr).queue_task_background(input_name.clone(), task);
        queued_stream_count += 1;
    }

    if queued_live_count > 0 || queued_stream_count > 0 {
        info!(
            "Queued probe tasks for input {input_name} (live_interval={queued_live_count}, generic={queued_stream_count})"
        );
    }
}

pub fn process_favourites(playlist: &mut Vec<PlaylistGroup>, favourites_cfg: Option<&[ConfigFavourites]>) {
    if let Some(favourites) = favourites_cfg {
        let mut fav_groups: IndexMap<CategoryKey, Vec<PlaylistItem>> = IndexMap::new();
        for pg in playlist.iter() {
            for pli in &pg.channels {
                // series episodes can't be included in favourites
                if pli.header.item_type == PlaylistItemType::Series
                    || pli.header.item_type == PlaylistItemType::LocalSeries
                {
                    continue;
                }
                for fav in favourites {
                    if pli.header.xtream_cluster == fav.cluster && is_valid(pli, &fav.filter, fav.match_as_ascii) {
                        let mut channel = pli.clone();
                        channel.header.group.clone_from(&fav.group);
                        // Update UUID to be an alias of the original
                        channel.header.uuid = create_alias_uuid(&pli.header.uuid, &fav.group);
                        fav_groups.entry((fav.cluster, fav.group.clone())).or_default().push(channel);
                    }
                }
            }
        }

        for (fav_group, channels) in fav_groups {
            if !channels.is_empty() {
                let (xtream_cluster, group_name) = fav_group;
                playlist.push(PlaylistGroup { id: 0, title: group_name, channels, xtream_cluster });
            }
        }
    }
}

pub(crate) async fn trakt_playlist(
    client: &reqwest::Client,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    playlist: &mut Vec<PlaylistGroup>,
) -> bool {
    match process_trakt_categories_for_target(client, playlist, target).await {
        Ok(Some(trakt_categories)) => {
            if !trakt_categories.is_empty() {
                info!("Adding {} Trakt categories to playlist", trakt_categories.len());
                playlist.extend(trakt_categories);
            }
        }
        Ok(None) => {
            return false;
        }
        Err(trakt_errors) => {
            warn!("Trakt processing failed with {} errors", trakt_errors.len());
            errors.extend(trakt_errors);
        }
    }
    true
}

pub(crate) async fn process_watch<E: EventSink>(
    app_config: &Arc<AppConfig>,
    events: &E,
    target: &ConfigTarget,
    new_playlist: &[PlaylistGroup],
) -> bool {
    let Some(watches) = &target.watch else {
        return false;
    };

    // Configured, but every pattern failed to compile. Silently doing
    // nothing here is what made a typo in `watch` indistinguishable from a
    // playlist that never changes.
    if watches.is_empty() {
        error!("target '{}' configured watch patterns but none of them compiled", target.name);
        events.emit(EventMessage::PlaylistWatchDisabled(WatchDisabled::new(
            target.name.clone(),
            WatchDisabledReason::InvalidPatterns,
        )));
        return false;
    }

    if default_as_default().eq_ignore_ascii_case(&target.name) {
        error!("can't watch a target with no unique name");
        events.emit(EventMessage::PlaylistWatchDisabled(WatchDisabled::new(
            target.name.clone(),
            WatchDisabledReason::UnnamedTarget,
        )));
        return false;
    }

    // Before the per-group fan-out: this is about which groups exist, not
    // what is inside the ones the patterns name, so it must see every group
    // rather than only the watched ones.
    process_target_groups_watch(app_config, events, &target.name, new_playlist).await;

    let mut matched = vec![false; watches.len()];
    let mut watched_groups = Vec::new();
    for group in new_playlist {
        let mut any = false;
        for (index, pattern) in watches.iter().enumerate() {
            if pattern.is_match(&group.title) {
                matched[index] = true;
                any = true;
            }
        }
        if any {
            watched_groups.push(group);
        }
    }

    // A pattern that matches nothing looks exactly like a group that has not
    // changed. `EventKindMask::from_wire_names` already reports unmatched
    // subscription names for the same reason: a typo must surface.
    let unmatched: Vec<String> = watches
        .iter()
        .enumerate()
        .filter(|(index, _)| !matched[*index])
        .map(|(_, pattern)| pattern.as_str().to_string())
        .collect();
    if !unmatched.is_empty() {
        warn!("target '{}' has {} watch pattern(s) matching no group", target.name, unmatched.len());
        events.emit(EventMessage::PlaylistWatchUnmatched(WatchUnmatched::new(
            target.name.clone(),
            unmatched,
            new_playlist.len(),
        )));
    }

    futures::stream::iter(
        watched_groups.into_iter().map(|pl| process_group_watch(app_config, events, &target.name, pl)),
    )
    .for_each_concurrent(16, |f| f)
    .await;

    true
}
