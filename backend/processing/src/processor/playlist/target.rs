#![allow(clippy::wildcard_imports)]
use super::{
    overrides::{
        reapply_target_overrides_ext, regroup_moved_items, resolve_item_uuid, InputOverrideSet, TargetOverrideSet,
    },
    *,
};
use shared::model::ItemOverride;

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

pub(crate) fn check_duplicate_manual_chno(
    target_name: &str,
    playlist: &[PlaylistGroup],
    locked: &HashMap<UUIDType, Arc<ItemOverride>>,
) -> Vec<u32> {
    if locked.is_empty() {
        return Vec::new();
    }
    let mut counts: HashMap<u32, usize> = HashMap::new();
    let mut locked_chnos: HashSet<u32> = HashSet::new();

    for group in playlist {
        for chan in &group.channels {
            // A copy of a group channel intentionally shares the group's locked number; only the
            // group channel itself takes part in the duplicate check.
            let is_group_copy =
                chan.header.failover_group_uuid.is_some_and(|group_uuid| group_uuid != chan.header.uuid);
            if is_group_copy {
                continue;
            }
            if chan.header.chno != 0 {
                *counts.entry(chan.header.chno).or_default() += 1;
                if let Some(uuid) = resolve_item_uuid(chan) {
                    if locked.contains_key(&uuid) {
                        locked_chnos.insert(chan.header.chno);
                    }
                }
            }
        }
    }

    let mut duplicates: Vec<u32> = counts
        .into_iter()
        .filter(|(chno, count)| *count > 1 && locked_chnos.contains(chno))
        .map(|(chno, _)| chno)
        .collect();
    duplicates.sort_unstable();

    for chno in &duplicates {
        log::warn!("Target '{target_name}': duplicate manual channel number {chno}");
    }
    duplicates
}

pub(crate) fn reapply_to_groups(groups: &mut Vec<PlaylistGroup>, locked: &HashMap<UUIDType, Arc<ItemOverride>>) {
    reapply_to_groups_ext(groups, locked, false);
}

pub(crate) fn reapply_to_groups_skip_chno(
    groups: &mut Vec<PlaylistGroup>,
    locked: &HashMap<UUIDType, Arc<ItemOverride>>,
) {
    reapply_to_groups_ext(groups, locked, true);
}

pub(crate) fn reapply_to_groups_ext(
    groups: &mut Vec<PlaylistGroup>,
    locked: &HashMap<UUIDType, Arc<ItemOverride>>,
    skip_chno: bool,
) {
    if locked.is_empty() {
        return;
    }
    if reapply_target_overrides_ext(groups.iter_mut().flat_map(|g| &mut g.channels), locked, skip_chno) {
        regroup_moved_items(groups, locked);
    }
}

pub(crate) fn reapply_to_fpl(fpl: &mut FetchedPlaylist<'_>, locked: &HashMap<UUIDType, Arc<ItemOverride>>) {
    if locked.is_empty() {
        return;
    }
    debug_assert!(fpl.is_memory(), "reapply_to_fpl called on non-memory playlist");
    let mut groups = fpl.source.take_groups();
    reapply_to_groups(&mut groups, locked);
    fpl.source = MemoryPlaylistSource::new(groups).into_source();
}

pub(crate) async fn record_target_overrides_status<
    E: EventSink + Clone + 'static,
    M: MetadataUpdateSink,
    P: PluginHost,
>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    target_name: &str,
    overrides: Option<&TargetOverrideSet>,
) {
    let Some(ref store) = ctx.override_status else {
        return;
    };
    let files_exist = overrides.is_some_and(|o| !o.has_no_files());
    if !files_exist && !store.has_target(target_name).await {
        return;
    }
    let (revisions, diagnostics) = match overrides {
        Some(o) => (
            o.revisions.clone(),
            tuliprox_repository::OverrideRunDiagnostics::new(
                o.applied_files.clone(),
                o.skipped_files.clone(),
                o.ambiguous_keys_count(),
            ),
        ),
        None => (Vec::new(), tuliprox_repository::OverrideRunDiagnostics::default()),
    };
    if let Err(err) = store.record_success(target_name, revisions, diagnostics).await {
        log::warn!("Failed to record override status for target '{target_name}': {err}");
    }
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
            tuliprox_core::model::TargetOutput::Xtream(_)
            | tuliprox_core::model::TargetOutput::HdHomeRun(_)
            | tuliprox_core::model::TargetOutput::Plugin(_) => {}
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
    pub(crate) overrides: Option<Arc<TargetOverrideSet>>,
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
#[allow(clippy::too_many_arguments, reason = "input_plugins joins the existing per-run inputs")]
pub(crate) async fn process_targets<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &Arc<PlaylistProcessingContext<E, M, P>>,
    playlists: &mut [FetchedPlaylist<'_>],
    targets: &[&Arc<ConfigTarget>],
    input_stats: &mut HashMap<Arc<str>, InputStats>,
    errors: &mut Vec<TuliproxError>,
    accepted_empty_clusters: ClusterFlags,
    process_parallel: bool,
    input_plugins: &HashMap<Arc<str>, PluginRunReport>,
) -> Vec<TargetStats> {
    let mut analytics_info = collect_source_analytics_info(playlists);
    let storage_dir = {
        let config = ctx.config.config.load();
        config.storage_dir.clone()
    };
    let mut input_statuses = HashMap::with_capacity(analytics_info.inputs.len());
    for input_info in &analytics_info.inputs {
        if input_info.epg.is_some() {
            let input_storage = tuliprox_repository::build_input_storage_path(&input_info.name, &storage_dir);
            let status = crate::input_cache::load_input_status_async(&input_storage).await;
            input_statuses.insert(input_info.name.clone(), status);
        }
    }
    analytics_info.input_statuses = Arc::new(input_statuses);
    let analytics_info = Arc::new(analytics_info);
    let shared_input_overrides = Arc::new(InputOverrideSet::load(&ctx.config).await);
    if !process_parallel {
        let mut target_stats = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let consume_input_source = index + 1 == targets.len();
            let prepare_started = Instant::now();
            let result = prepare_playlist_for_target(
                ctx,
                playlists,
                target,
                input_stats,
                errors,
                accepted_empty_clusters,
                consume_input_source,
                input_plugins,
                &analytics_info,
                &shared_input_overrides,
            )
            .await;
            match result {
                Ok(prepared) => {
                    let mut processing = prepared.processing.clone();
                    let overrides = Arc::clone(&prepared.overrides);
                    let (result, mut finalization_errors, finalize_report) =
                        finalize_prepared_target(Arc::clone(ctx), prepared).await;
                    processing.plugins.merge(finalize_report);
                    errors.append(&mut finalization_errors);
                    match result {
                        Ok(()) => {
                            record_target_overrides_status(ctx, &target.name, Some(&overrides)).await;
                            target_stats.push(TargetStats::success_with_processing(&target.name, processing));
                        }
                        Err(mut target_errors) => {
                            target_stats.push(TargetStats::failure_with_processing(&target.name, processing));
                            errors.append(&mut target_errors);
                        }
                    }
                }
                Err(mut target_errors) => {
                    record_failed_target_analytics(
                        &ctx.config.config.load_full(),
                        &target.name,
                        &analytics_info,
                        prepare_started,
                    )
                    .await;
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

        let prepare_started = Instant::now();
        match prepare_playlist_for_target(
            ctx,
            playlists,
            target,
            input_stats,
            errors,
            accepted_empty_clusters,
            false,
            input_plugins,
            &analytics_info,
            &shared_input_overrides,
        )
        .await
        {
            Ok(prepared) => {
                let processing = prepared.processing.clone();
                let overrides = Arc::clone(&prepared.overrides);
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
                        Ok((result, errors, finalize_report)) => {
                            let mut processing = processing;
                            processing.plugins.merge(finalize_report);
                            TargetJobResult {
                                index,
                                name: target_name,
                                result,
                                errors,
                                processing,
                                overrides: Some(overrides),
                            }
                        }
                        Err(_) => TargetJobResult {
                            index,
                            name: target_name.clone(),
                            result: Err(vec![TuliproxError::RepositoryPlaylist(format!(
                                "Target '{target_name}' finalization panicked"
                            ))]),
                            errors: Vec::new(),
                            processing,
                            overrides: Some(overrides),
                        },
                    }
                });
            }
            Err(target_errors) => {
                completion.send_replace(true);
                record_failed_target_analytics(
                    &ctx.config.config.load_full(),
                    &target.name,
                    &analytics_info,
                    prepare_started,
                )
                .await;
                results.push(TargetJobResult {
                    index,
                    name: target.name.clone(),
                    result: Err(target_errors),
                    errors: Vec::new(),
                    processing: PipelineStats::default(),
                    overrides: None,
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
                record_target_overrides_status(ctx, &target_result.name, target_result.overrides.as_deref()).await;
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
    pub(crate) library_empty: tuliprox_repository::LibraryEmptyPublication,
    pub(crate) target: ConfigTarget,
    pub(crate) playlist: Vec<PlaylistGroup>,
    pub(crate) epg: Vec<Epg>,
    pub(crate) processing: PipelineStats,
    pub(crate) accepted_empty_clusters: ClusterFlags,
    pub(crate) analytics_input: TargetAnalyticsInput,
    pub(crate) overrides: Arc<TargetOverrideSet>,
}

async fn load_target_bouquet_filter<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    target_name: &str,
) -> Result<Option<tuliprox_core::model::TargetBouquetFilter>, TuliproxError> {
    let bouquet_file = tuliprox_repository::load_target_bouquet(&ctx.config, target_name).await?;
    let bouquet_filter =
        bouquet_file.and_then(|file| tuliprox_core::model::TargetBouquetFilter::from_dto(file.bouquet));
    if let Some(ref filter) = bouquet_filter {
        let (live, vod, series) = filter.cluster_counts();
        debug!("Loaded target bouquet for '{target_name}': live={live:?}, vod={vod:?}, series={series:?}");
    }
    Ok(bouquet_filter)
}

/// A target's processing filter may call `plugin.<id>.<fn>(…)`. When the plugin is not installed or not
/// enabled, nothing can answer that term: the host's default is `false` for every item, so the filter
/// would drop the whole playlist and still report the target as successful. Fail the target instead —
/// it keeps its previous output, and the log names the plugin and the fix.
fn ensure_filter_plugins_available<P: PluginHost>(plugins: &P, target: &ConfigTarget) -> Result<(), TuliproxError> {
    let Some(filter) = target.filter.processing.as_ref() else {
        return Ok(());
    };
    for call in filter.plugin_calls() {
        if !plugins.provides(PluginHook::FilterEval, call.plugin) {
            return Err(TuliproxError::ConfigTarget(format!(
                "target '{}' is inactive: its filter calls plugin '{}', which is not installed or not enabled; \
                 the target keeps its previous playlist",
                target.name, call.plugin
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(crate) async fn prepare_playlist_for_target<
    E: EventSink + Clone + 'static,
    M: MetadataUpdateSink,
    P: PluginHost,
>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    playlists: &mut [FetchedPlaylist<'_>],
    target: &ConfigTarget,
    stats: &mut HashMap<Arc<str>, InputStats>,
    errors: &mut Vec<TuliproxError>,
    accepted_empty_clusters: ClusterFlags,
    consume_input_source: bool,
    input_plugins: &HashMap<Arc<str>, PluginRunReport>,
    analytics_info: &Arc<SourceAnalyticsInfo>,
    input_overrides: &Arc<InputOverrideSet>,
) -> Result<PreparedTarget, Vec<TuliproxError>> {
    debug_if_enabled!("Processing order is {}", &target.processing_order);
    log_memory_snapshot(format!("target '{}' start", target.name).as_str());

    let mut duplicates: HashSet<UUIDType> = HashSet::new();
    let mut new_epg = vec![];
    let mut new_playlist: Vec<PlaylistGroup> = vec![];
    let mut aggregate_outcome = PipelineOutcome::default();
    let mut has_empty_library = false;
    let mut all_inputs_empty = !playlists.is_empty();
    let mut only_library_inputs = true;
    // The caller has already required Ready for every input. An empty foreign input
    // still needs its own Force contract; it cannot borrow Library's filter permission.
    let mut non_library_inputs_populated = true;

    let analytics_input = TargetAnalyticsInput { started: Instant::now(), source_info: Arc::clone(analytics_info) };

    debug!("Executing processing pipes");
    let broadcast_step = create_broadcast_callback(&ctx.events, &ctx.run_id, ctx.execution_order);
    ensure_filter_plugins_available(&ctx.plugins, target).map_err(|err| vec![err])?;
    let bouquet_filter = load_target_bouquet_filter(ctx, &target.name).await.map_err(|err| vec![err])?;
    let target_input_names: Vec<&str> = playlists.iter().map(|p| p.input.name.as_ref()).collect();
    let target_overrides =
        Arc::new(TargetOverrideSet::load_for_target(input_overrides, &ctx.config, target, &target_input_names).await);
    let pipe = get_processing_pipe(target);
    let origins = playlists.iter().map(|fpl| fpl.input).collect::<Vec<_>>();
    let mut managed = reconcile_target_managed(
        &ctx.plugins,
        target,
        &origins,
        bouquet_filter.as_ref(),
        &mut aggregate_outcome.plugins,
    )
    .await
    .map_err(|abort| vec![TuliproxError::Plugin(abort.to_string())])?;
    let mut step = StepMeasure::new(&target.name, broadcast_step);
    for provider_fpl in playlists.iter_mut() {
        let insertion = managed.remove(&provider_fpl.input.name).unwrap_or_default();
        let input_is_empty = provider_fpl.source.is_empty() && insertion.is_empty();
        all_inputs_empty &= input_is_empty;
        only_library_inputs &= provider_fpl.input.input_type == InputType::Library;
        non_library_inputs_populated &= provider_fpl.input.input_type == InputType::Library || !input_is_empty;
        has_empty_library |= provider_fpl.input.input_type == InputType::Library && input_is_empty;
        // `on-playlist-loaded` ran once for the input; every target that consumes it reports it once.
        if let Some(loaded) = input_plugins.get(&provider_fpl.input.name) {
            aggregate_outcome.plugins.merge(loaded.clone());
        }
        log_memory_snapshot(
            format!("target '{}' input '{}' before_pipe", target.name, provider_fpl.input.name).as_str(),
        );
        step.broadcast("Executing transformations on '{}' playlist", &target.name);
        let (mut processed_fpl, input_outcome) = execute_pipe_with_insertions(
            target,
            &pipe,
            provider_fpl,
            &mut duplicates,
            consume_input_source,
            bouquet_filter.as_ref(),
            &ctx.plugins,
            Some(&target_overrides),
            insertion,
        )
        .await
        .map_err(|err| vec![err])?;
        debug!("Target '{}' input '{}' pipeline outcome: {input_outcome:?}", target.name, provider_fpl.input.name);
        aggregate_outcome.merge(input_outcome);
        log_memory_snapshot(
            format!("target '{}' input '{}' after_pipe", target.name, provider_fpl.input.name).as_str(),
        );
        processed_fpl.sort_by_provider_ordinal();
        let series_report = playlist_resolve(ctx, target, errors, &pipe, provider_fpl, &mut processed_fpl).await;
        aggregate_outcome.plugins.merge(series_report);
        {
            let locked = target_overrides.target_locked();
            if !locked.is_empty() {
                reapply_to_fpl(&mut processed_fpl, &locked);
            }
        }
        log_memory_snapshot(
            format!("target '{}' input '{}' after_vod_resolve", target.name, provider_fpl.input.name).as_str(),
        );
        let clear_invalid_epg_ids = target.options.as_ref().is_some_and(ConfigTargetOptions::clear_invalid_epg_ids);
        let input_epg_start = new_epg.len();
        let epg_assign_options = EpgAssignOptions {
            // An input in maintenance runs no plugin hook that could reach its provider's EPG.
            plugin_hooks: !ctx.is_input_in_maintenance(&processed_fpl.input.name),
            ..EpgAssignOptions::from_target_options(target.options.as_ref())
        };
        let epg_report = process_playlist_epg(&ctx.plugins, &mut processed_fpl, &mut new_epg, epg_assign_options).await;
        aggregate_outcome.plugins.merge(epg_report);
        if epg_assign_options.plugin_hooks {
            enrich_playlist_programmes(
                &ctx.plugins,
                &HookCtx::new(&target.name).with_input(&processed_fpl.input.name),
                &mut new_epg[input_epg_start..],
                &mut aggregate_outcome.plugins,
            )
            .await;
        }
        {
            let locked = target_overrides.target_locked();
            if !locked.is_empty() {
                reapply_to_fpl(&mut processed_fpl, &locked);
            }
        }
        log_memory_snapshot(
            format!("target '{}' input '{}' after_epg_apply", target.name, processed_fpl.input.name).as_str(),
        );
        let deduplicate = target.execution_plan.pre_transform_identity_dedup;
        if let Some(mut groups) = map_playlist_at_stage(
            &mut processed_fpl.source,
            target,
            MappingStage::AfterEpg,
            deduplicate.then_some(&mut duplicates),
        ) {
            let locked = target_overrides.target_locked();
            if !locked.is_empty() {
                reapply_to_groups(&mut groups, &locked);
            }
            processed_fpl.source = MemoryPlaylistSource::new(groups).into_source();
        }
        if epg_assign_options.adopt_guide_id_case && processed_fpl.epg.is_some() {
            adopt_guide_live_epg_id_case(&mut processed_fpl, &new_epg[input_epg_start..]);
            let locked = target_overrides.target_locked();
            if !locked.is_empty() {
                reapply_to_fpl(&mut processed_fpl, &locked);
            }
        }
        if clear_invalid_epg_ids && processed_fpl.epg.is_some() {
            clear_invalid_live_epg_ids(&mut processed_fpl, &new_epg[input_epg_start..]);
            let locked = target_overrides.target_locked();
            if !locked.is_empty() {
                reapply_to_fpl(&mut processed_fpl, &locked);
            }
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
        library_empty: match (has_empty_library, all_inputs_empty && only_library_inputs) {
            (true, true) => tuliprox_repository::LibraryEmptyPublication::CompleteTarget,
            (true, false) if non_library_inputs_populated && accepted_empty_clusters.is_empty() => {
                tuliprox_repository::LibraryEmptyPublication::FilterableContribution
            }
            (true, false) => tuliprox_repository::LibraryEmptyPublication::Contribution,
            (false, _) => tuliprox_repository::LibraryEmptyPublication::None,
        },
        target: target.clone(),
        playlist: new_playlist,
        epg: new_epg,
        processing: aggregate_outcome.to_stats(),
        accepted_empty_clusters,
        analytics_input,
        overrides: target_overrides,
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

fn finalization_step_label(view: &str, stage: &str) -> String {
    if view.is_empty() {
        stage.to_string()
    } else {
        format!("{view} {stage}")
    }
}

/// The distinct group titles of `playlist`, in first-seen order.
fn group_titles(playlist: &[PlaylistGroup]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    playlist.iter().filter(|group| seen.insert(&group.title)).map(|group| group.title.to_string()).collect()
}

/// Apply a `merge-groups` decision: rename every member group (and its
/// channels' `group`) to its canonical title, so `flatten_groups` merges them.
/// Titles the playlist does not have are ignored, a member named under two
/// canonical titles goes to the first one. Returns the number of renamed groups.
pub(crate) fn apply_group_merge(playlist: &mut [PlaylistGroup], merge: &PluginGroupMerge) -> usize {
    let mut canonical_of: HashMap<&str, &str> = HashMap::new();
    for (canonical, members) in merge.iter().filter(|(canonical, _)| !canonical.trim().is_empty()) {
        for member in members {
            canonical_of.entry(member.as_str()).or_insert(canonical.as_str());
        }
    }
    let mut renamed = 0;
    for group in playlist {
        let Some(canonical) = canonical_of.get(&*group.title).copied().filter(|canonical| **canonical != *group.title)
        else {
            continue;
        };
        let title = canonical.intern();
        group.title = Arc::clone(&title);
        for channel in &mut group.channels {
            channel.header.group = Arc::clone(&title);
        }
        renamed += 1;
    }
    renamed
}

/// `resolve-duplicates` decision for the post-merge pass: one guest call per
/// duplicate group, aligned with [`group_duplicates`] of `playlist`.
async fn plugin_duplicate_keepers<P: PluginHost>(
    plugins: &P,
    plugin: &str,
    ctx: &HookCtx<'_>,
    dedup_config: &DeduplicateConfig,
    playlist: &[PlaylistGroup],
    skip: &HashSet<UUIDType>,
    report: &mut PluginRunReport,
) -> Result<Option<PluginKeepers>, PluginAbort> {
    let sets = group_duplicates(dedup_config, playlist, skip);
    if sets.is_empty() {
        return Ok(None);
    }
    let groups = plugin_duplicate_groups(playlist, &sets);
    plugins.resolve_duplicates(plugin, ctx, &groups, report).await
}

/// Flush the catalog into one merged, deduplicated playlist.
///
/// Async, with the two strategy hooks as prepare -> apply (plan §7
/// pattern 2): `merge-groups` renames members before `flatten_groups`, and
/// `resolve-duplicates` hands the keepers to the otherwise synchronous
/// `deduplicate_playlist`.
pub(crate) async fn prepare_eligible_catalog<P: PluginHost>(
    plugins: &P,
    target: &ConfigTarget,
    mut playlist: Vec<PlaylistGroup>,
    group_uuids: &HashSet<UUIDType>,
    step: &mut StepMeasure,
    report: &mut PluginRunReport,
) -> Result<Vec<PlaylistGroup>, PluginAbort> {
    let ctx = HookCtx::new(&target.name);
    let strategies = &target.execution_plan.plugin_strategies;
    if let Some(plugin) = &strategies.merge_groups {
        let titles = group_titles(&playlist);
        if !titles.is_empty() {
            if let Some(merge) = plugins.merge_groups(plugin.id(), &ctx, &titles, report).await? {
                let renamed = apply_group_merge(&mut playlist, &merge);
                debug!("Target '{}': plugin {} merged {renamed} groups", target.name, plugin.id());
            }
        }
    }
    let mut playlist = flatten_groups(playlist);
    step.tick("playlist merge");
    log_memory_snapshot(format!("target '{}' after_playlist_merge", target.name).as_str());
    if let Some(dedup_config) = target.execution_plan.post_merge_content_dedup.as_ref() {
        let keepers = match &strategies.resolve_duplicates {
            Some(plugin) => {
                plugin_duplicate_keepers(plugins, plugin.id(), &ctx, dedup_config, &playlist, group_uuids, report)
                    .await?
            }
            None => None,
        };
        let removed = crate::processor::deduplicate::deduplicate_playlist(
            dedup_config,
            &mut playlist,
            keepers.as_ref(),
            group_uuids,
        );
        if removed > 0 {
            info!("Deduplicated {removed} eligible channels for target {}", target.name);
        }
        step.tick("playlist dedup");
        log_memory_snapshot(format!("target '{}' after_playlist_dedup", target.name).as_str());
    }
    Ok(playlist)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaylistViewKind {
    Base,
    Xtream,
}

impl PlaylistViewKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Xtream => "Xtream",
        }
    }

    pub(crate) fn warns_duplicates(self) -> bool { matches!(self, Self::Base) }
}

/// Every channel of a view, in view order: what the finalize-side hooks index.
fn flat_items(playlist: &[PlaylistGroup]) -> Vec<&PlaylistItem> {
    playlist.iter().flat_map(|group| group.channels.iter()).collect()
}

/// `sort-key` decision for one view: one key per channel in `flat_items` order.
async fn plugin_sort_keys<P: PluginHost>(
    plugins: &P,
    plugin: &str,
    ctx: &HookCtx<'_>,
    playlist: &[PlaylistGroup],
    report: &mut PluginRunReport,
) -> Result<Option<PluginSortKeys>, PluginAbort> {
    let items = flat_items(playlist);
    if items.is_empty() {
        return Ok(None);
    }
    plugins.sort_keys(plugin, ctx, &items, report).await
}

/// Apply an `after-sort` order, a permutation of the view's flat channel
/// indices (`order[k]` = the channel that comes `k`-th). Channels reorder
/// within their group by rank and groups by their first channel's rank, so a
/// plugin cannot move a channel into another group. An order that is not a
/// permutation of `0..channels` changes nothing.
pub(crate) fn apply_after_sort(playlist: &mut Vec<PlaylistGroup>, order: &[usize]) -> Result<(), String> {
    let total: usize = playlist.iter().map(|group| group.channels.len()).sum();
    if order.len() != total {
        return Err(format!("{} indices for {total} channels", order.len()));
    }
    let mut rank = vec![usize::MAX; total];
    for (position, &index) in order.iter().enumerate() {
        match rank.get_mut(index) {
            None => return Err(format!("index {index} is out of bounds")),
            Some(slot) if *slot != usize::MAX => return Err(format!("duplicate index {index}")),
            Some(slot) => *slot = position,
        }
    }

    let mut offset = 0;
    let mut keyed = Vec::with_capacity(playlist.len());
    for mut group in std::mem::take(playlist) {
        let len = group.channels.len();
        let ranks = &rank[offset..offset + len];
        offset += len;
        let mut indices: Vec<usize> = (0..len).collect();
        indices.sort_by_key(|index| ranks[*index]);
        let first_rank = indices.first().map_or(usize::MAX, |index| ranks[*index]);
        let mut channels: Vec<Option<PlaylistItem>> =
            std::mem::take(&mut group.channels).into_iter().map(Some).collect();
        group.channels = indices.iter().filter_map(|index| channels[*index].take()).collect();
        keyed.push((first_rank, group));
    }
    keyed.sort_by_key(|(first_rank, _)| *first_rank);
    *playlist = keyed.into_iter().map(|(_, group)| group).collect();
    Ok(())
}

/// A channel's identity across the views of one target: its uuid and how many
/// channels with that uuid came before it in the view.
type ChnoKey = (UUIDType, u32);

/// Plugin channel numbers of one target, computed once for the first view and
/// reused for the second so a channel has one number in both.
type PluginChnos = Option<HashMap<ChnoKey, u32>>;

fn chno_keys(items: &[&PlaylistItem]) -> Vec<ChnoKey> {
    let mut seen: HashMap<UUIDType, u32> = HashMap::new();
    items
        .iter()
        .map(|item| {
            let uuid = *item.header.get_uuid();
            let seen_before = seen.entry(uuid).or_insert(0);
            let key = (uuid, *seen_before);
            *seen_before += 1;
            key
        })
        .collect()
}

/// `assign-channel-numbers` decision for the first view, keyed by item
/// identity. Zero chnos and indices outside the view are dropped.
async fn plugin_chno_numbers<P: PluginHost>(
    plugins: &P,
    plugin: &str,
    ctx: &HookCtx<'_>,
    playlist: &[PlaylistGroup],
    report: &mut PluginRunReport,
) -> Result<HashMap<ChnoKey, u32>, PluginAbort> {
    let items = flat_items(playlist);
    if items.is_empty() {
        return Ok(HashMap::new());
    }
    let Some(chnos) = plugins.assign_chno(plugin, ctx, &items, report).await? else {
        return Ok(HashMap::new());
    };
    let keys = chno_keys(&items);
    Ok(chnos
        .into_iter()
        .filter(|(_, chno)| *chno != 0)
        .filter_map(|(index, chno)| keys.get(index).map(|key| (*key, chno)))
        .collect())
}

/// Give the listed channels their plugin number; the host numbers the rest
/// (`assign_channel_no_playlist` skips every number already taken).
pub(crate) fn apply_plugin_chnos(playlist: &mut [PlaylistGroup], numbers: &HashMap<ChnoKey, u32>) -> usize {
    let keys = chno_keys(&flat_items(playlist));
    let mut assigned = 0;
    for (channel, key) in playlist.iter_mut().flat_map(|group| group.channels.iter_mut()).zip(keys) {
        if let Some(chno) = numbers.get(&key) {
            channel.header.chno = *chno;
            assigned += 1;
        }
    }
    assigned
}

/// Runs once per view (base, and Xtream when curation emits one).
///
/// Async per plan §7 *Sync/async boundary*, pattern 1 ("lift the loop, keep the
/// work sync"): the stage arms stay sync, and `after-sort` is awaited between
/// the `Sort` and `AssignChannelNumbers` arms. `sort-key` and
/// `assign-channel-numbers` are prepare -> apply data computed here, before the
/// sync arm that consumes them.
///
/// `locked` holds the target's manual overrides, re-applied after the appearance merge and
/// before channel numbers are assigned so no later stage can undo them.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_playlist_view<P: PluginHost>(
    plugins: &P,
    target: &ConfigTarget,
    playlist: Vec<PlaylistGroup>,
    step: &mut StepMeasure,
    view_kind: PlaylistViewKind,
    chnos: &mut PluginChnos,
    report: &mut PluginRunReport,
    locked: Option<&HashMap<UUIDType, Arc<ItemOverride>>>,
) -> Result<Vec<PlaylistGroup>, PluginAbort> {
    let view = view_kind.label();
    let mut playlist = flatten_groups(playlist);
    step.tick(&finalization_step_label(view, "appearance merge"));
    if let Some(locked) = locked.filter(|l| !l.is_empty()) {
        reapply_to_groups(&mut playlist, locked);
    }
    let ctx = HookCtx::new(&target.name).with_view(view);
    let strategies = &target.execution_plan.plugin_strategies;

    for stage in FINALIZATION_ORDER.into_iter().skip(2) {
        match stage {
            FinalizationStage::Merge | FinalizationStage::Deduplicate => {
                unreachable!("eligibility merge and deduplication are completed before curation")
            }
            FinalizationStage::Sort => {
                let keys = match &strategies.sort_key {
                    Some(plugin) => plugin_sort_keys(plugins, plugin.id(), &ctx, &playlist, report).await?,
                    None => None,
                };
                if sort_playlist(target, &mut playlist, keys.as_ref()) {
                    step.tick(&finalization_step_label(view, "playlist sort"));
                    log_memory_snapshot(format!("target '{}' {view} after_playlist_sort", target.name).as_str());
                }
                if plugins.wants(PluginHook::AfterSort, &target.name) {
                    let order = {
                        let items = flat_items(&playlist);
                        if items.is_empty() {
                            None
                        } else {
                            plugins.after_sort(&ctx, &items, report).await?
                        }
                    };
                    if let Some(order) = order {
                        match apply_after_sort(&mut playlist, &order) {
                            Ok(()) => step.tick(&finalization_step_label(view, "plugin sort")),
                            Err(err) => warn!(
                                "Target '{}' {view}: after-sort returned an invalid order ({err}); keeping the host order",
                                target.name
                            ),
                        }
                    }
                }
            }
            FinalizationStage::AssignChannelNumbers => {
                if let Some(plugin) = &strategies.assign_chno {
                    if chnos.is_none() {
                        *chnos = Some(plugin_chno_numbers(plugins, plugin.id(), &ctx, &playlist, report).await?);
                    }
                    if let Some(numbers) = chnos.as_ref().filter(|numbers| !numbers.is_empty()) {
                        apply_plugin_chnos(&mut playlist, numbers);
                    }
                }
                if let Some(locked) = locked.filter(|l| !l.is_empty()) {
                    reapply_to_groups(&mut playlist, locked);
                }
                assign_channel_no_playlist(&mut playlist);
                if view_kind.warns_duplicates() {
                    if let Some(locked) = locked.filter(|l| !l.is_empty()) {
                        check_duplicate_manual_chno(&target.name, &playlist, locked);
                    }
                }
                step.tick(&finalization_step_label(view_kind.label(), "assigning channel numbers"));
                log_memory_snapshot(
                    format!("target '{}' {} after_assign_channel_numbers", target.name, view_kind.label()).as_str(),
                );
            }
            FinalizationStage::AssignCounters => {
                map_playlist_counter(target, &mut playlist);
                if let Some(locked) = locked.filter(|l| !l.is_empty()) {
                    reapply_to_groups_skip_chno(&mut playlist, locked);
                }
                step.tick(&finalization_step_label(view_kind.label(), "assigning channel counter"));
                log_memory_snapshot(
                    format!("target '{}' {} after_assign_channel_counter", target.name, view_kind.label()).as_str(),
                );
            }
        }
    }

    apply_persist_filter(target, &mut playlist);
    Ok(playlist)
}

// Keep construction and its diagnostic seam private to the processing boundary.
// Only fixed phase labels can reach the reporter, never dependency error details.
pub(super) fn build_target_tmdb_client(
    target: &ConfigTarget,
    configure: impl FnOnce() -> Result<reqwest::ClientBuilder, TuliproxError>,
    report: impl FnOnce(&'static str),
) -> Option<reqwest::Client> {
    let needs_tmdb = target.curation.as_ref().is_some_and(|config| {
        config.enabled && config.tmdb.as_ref().is_some_and(|source| source.enabled && !source.trending.is_empty())
    });
    if !needs_tmdb {
        return None;
    }
    let Ok(builder) = configure() else {
        report("phase=profile_configuration");
        return None;
    };
    if let Ok(client) = builder.build() {
        Some(client)
    } else {
        report("phase=client_build");
        None
    }
}

/// What finalizing a target produced: the persist result, the errors that do
/// not fail the target, and what plugins did on its finalize-side hooks.
pub(crate) type FinalizeOutcome = (Result<(), Vec<TuliproxError>>, Vec<TuliproxError>, PluginRunReport);

pub(crate) async fn finalize_prepared_target<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: Arc<PlaylistProcessingContext<E, M, P>>,
    prepared: PreparedTarget,
) -> FinalizeOutcome {
    // Keep TMDB transport settings independent of the generic client.
    let tmdb_client = build_target_tmdb_client(
        &prepared.target,
        || tuliprox_core::utils::network::request::create_tmdb_client(&ctx.config),
        |phase| {
            warn!("TMDB discovery client unavailable: target_id={} run_id={} {phase}", prepared.target.id, ctx.run_id);
        },
    );
    finalize_prepared_target_with_tmdb(ctx, prepared, tmdb_client.as_ref()).await
}

/// A group channel replaces the representative's uuid with the group uuid, so a target override
/// locked on the representative is unreachable under the group uuid. Register the same override
/// for the group uuid; copies of the group channel resolve it through their `failover_group_uuid`
/// in the re-apply path, so they need no lock entry of their own.
pub(crate) fn lock_group_representatives(
    pending: &[tuliprox_repository::PendingFailoverGroup],
    locked: &mut HashMap<UUIDType, Arc<ItemOverride>>,
) {
    if locked.is_empty() {
        return;
    }
    for group in pending {
        let Some(representative) = group.members.first() else {
            continue;
        };
        let Some(representative_uuid) = resolve_item_uuid(representative) else {
            continue;
        };
        if let Some(item_override) = locked.get(&representative_uuid).map(Arc::clone) {
            locked.insert(group.group_uuid, item_override);
        }
    }
}

pub(super) fn process_target_failover_groups(
    config: &tuliprox_core::model::AppConfig,
    target: &ConfigTarget,
    playlist: &mut Vec<PlaylistGroup>,
    step: &mut StepMeasure,
    errors: &mut Vec<TuliproxError>,
) -> FailoverGroupBatch {
    let Some(target_cfg) = target.execution_plan.failover_groups.as_ref() else {
        return FailoverGroupBatch::default();
    };
    let cg_cfg = config.config.load().failover_groups.for_target(target_cfg);
    // One snapshot for error and section, so the meta never mixes two failover generations.
    let failover = config.failover.load();
    if let Some(error) = failover.error.as_deref() {
        errors.push(TuliproxError::FailoverGroup(format!(
            "Failover overrides are not applied to target '{}': {error}",
            target.name
        )));
    }
    let overrides = failover.sections.get(target.name.as_str());
    step.broadcast("Processing failover groups for '{}' playlist", &target.name);
    let mut batch = crate::processor::failover_group::group_playlist(
        &cg_cfg,
        overrides.map(|entry| &entry.overrides),
        &target.name,
        playlist,
    );
    batch.meta = FailoverGroupMeta {
        overrides_etag: overrides.map(|entry| Arc::clone(&entry.etag)),
        overrides_error: failover.error.clone(),
        applied_overrides: overrides.map(|entry| entry.overrides.clone()),
        // A publication shows the current representatives, so nothing is stale after it.
        display_stale_groups: Vec::new(),
    };
    step.tick("failover grouping");
    log_memory_snapshot(format!("target '{}' after_failover_grouping", target.name).as_str());
    batch
}

/// Nothing is published: keep the previous metrics and CSV, only record the run.
async fn record_unchanged_target_analytics(
    config: &tuliprox_core::model::Config,
    target_name: &str,
    analytics_input: &TargetAnalyticsInput,
) {
    let (empty_analytics, empty_csv) = compute_target_analytics(&config.storage_dir, analytics_input, &[], &[]);
    persist_target_analytics(config, target_name, empty_analytics, &empty_csv, TargetAnalyticsOutcome::Unchanged).await;
}

pub(super) async fn finalize_prepared_target_with_tmdb<
    E: EventSink + Clone + 'static,
    M: MetadataUpdateSink,
    P: PluginHost,
>(
    ctx: Arc<PlaylistProcessingContext<E, M, P>>,
    prepared: PreparedTarget,
    tmdb_client: Option<&reqwest::Client>,
) -> FinalizeOutcome {
    let mut report = PluginRunReport::default();
    let (result, errors) = finalize_target_views(ctx, prepared, tmdb_client, &mut report).await;
    (result, errors, report)
}

#[allow(clippy::too_many_lines)]
async fn finalize_target_views<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: Arc<PlaylistProcessingContext<E, M, P>>,
    prepared: PreparedTarget,
    tmdb_client: Option<&reqwest::Client>,
    report: &mut PluginRunReport,
) -> (Result<(), Vec<TuliproxError>>, Vec<TuliproxError>) {
    let config = ctx.config.config.load_full();
    let target = &prepared.target;
    let mut new_playlist = prepared.playlist;
    let mut new_epg = prepared.epg;
    let mut errors = Vec::new();
    let broadcast_step = create_broadcast_callback(&ctx.events, &ctx.run_id, ctx.execution_order);
    let mut step = StepMeasure::new(&target.name, broadcast_step);

    let pending_failover_groups =
        process_target_failover_groups(&ctx.config, target, &mut new_playlist, &mut step, &mut errors);
    lock_group_representatives(&pending_failover_groups.groups, &mut prepared.overrides.target_locked.lock());

    if target.favourites.is_some() {
        step.broadcast("Processing favourites for '{}' playlist", &target.name);
        process_favourites(&mut new_playlist, target.favourites.as_deref());
        log_memory_snapshot(format!("target '{}' after_favourites", target.name).as_str());
    }

    let library_empty = prepared.library_empty.for_filtered_playlist(&new_playlist);
    if new_playlist.is_empty() && prepared.accepted_empty_clusters.is_empty() && !library_empty.replaces_empty_target()
    {
        step.stop("");
        info!("Playlist is empty: {}", target.name);
        record_unchanged_target_analytics(&config, &target.name, &prepared.analytics_input).await;
        return (Ok(()), errors);
    }

    let group_uuids: HashSet<UUIDType> = pending_failover_groups.groups.iter().map(|p| p.group_uuid).collect();
    let eligible_catalog =
        match prepare_eligible_catalog(&ctx.plugins, target, new_playlist, &group_uuids, &mut step, report).await {
            Ok(catalog) => catalog,
            Err(abort) => {
                step.stop("");
                return (Err(vec![TuliproxError::Plugin(abort.to_string())]), errors);
            }
        };
    let mut views = prepare_target_playlist_views(&ctx.client, tmdb_client, target, eligible_catalog).await;
    views.failover_groups = pending_failover_groups;

    if views.base.is_empty()
        && views.xtream.is_none()
        && views.publication_plan == PlaylistPublicationPlan::Ordinary
        && prepared.accepted_empty_clusters.is_empty()
        && !library_empty.replaces_empty_target()
    {
        step.stop("");
        info!("Playlist is empty: {}", target.name);
        record_unchanged_target_analytics(&config, &target.name, &prepared.analytics_input).await;
        return (Ok(()), errors);
    }

    let locked_map = prepared.overrides.target_locked.lock().clone();
    let locked = if locked_map.is_empty() { None } else { Some(&locked_map) };
    if let Some(locked) = locked {
        reapply_to_groups(&mut views.base, locked);
        if let Some(ref mut xtream) = views.xtream {
            reapply_to_groups(xtream, locked);
        }
    }
    if views.publication_plan != PlaylistPublicationPlan::Ordinary {
        step.tick("target curation");
        log_memory_snapshot(format!("target '{}' after_curation", target.name).as_str());
    }

    // Plugin channel numbers are computed on the first view and reused on the second.
    let mut chnos = PluginChnos::default();
    let finalized = finalize_playlist_view(
        &ctx.plugins,
        target,
        views.base,
        &mut step,
        PlaylistViewKind::Base,
        &mut chnos,
        report,
        locked,
    )
    .await;
    let mut finalized_base = match finalized {
        Ok(playlist) => playlist,
        Err(abort) => {
            step.stop("");
            return (Err(vec![TuliproxError::Plugin(abort.to_string())]), errors);
        }
    };
    let mut finalized_xtream = match views.xtream {
        Some(playlist) => {
            match finalize_playlist_view(
                &ctx.plugins,
                target,
                playlist,
                &mut step,
                PlaylistViewKind::Xtream,
                &mut chnos,
                report,
                locked,
            )
            .await
            {
                Ok(playlist) => Some(playlist),
                Err(abort) => {
                    step.stop("");
                    return (Err(vec![TuliproxError::Plugin(abort.to_string())]), errors);
                }
            }
        }
        None => None,
    };
    drop(locked_map);
    retain_epg_referenced_by_groups(&finalized_base, &mut new_epg);

    let watch_playlist = target_watch_view(&finalized_base, finalized_xtream.as_deref());
    let (analytics_dto, unmapped_csv) =
        compute_target_analytics(&config.storage_dir, &prepared.analytics_input, watch_playlist, &new_epg);

    let merged_epg = if config.disk_based_processing {
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
                persist_target_analytics(
                    &config,
                    &target.name,
                    analytics_dto,
                    &unmapped_csv,
                    TargetAnalyticsOutcome::Failed,
                )
                .await;
                step.stop("EPG spill failed; skipping persist to preserve existing EPG");
                log_memory_snapshot(format!("target '{}' after_persist", target.name).as_str());
                return (Err(vec![result_error]), errors);
            }
        }
    } else {
        flatten_tvguide(new_epg)
    };
    let library_empty = library_empty.for_filtered_playlist(&finalized_base);
    let result = persist_playlist_views_with_groups(
        &ctx.config,
        &mut finalized_base,
        finalized_xtream.as_deref_mut(),
        merged_epg.as_ref(),
        target,
        ctx.playlist_state.as_ref(),
        TargetPlaylistPersistOptions {
            accepted_empty_clusters: prepared.accepted_empty_clusters,
            library_empty,
            publication_plan: views.publication_plan,
        },
        &mut views.failover_groups,
    )
    .await;
    // `type: plugin` outputs (plan §7 *Output providers*): the repository has no
    // plugin seam, so they render here, each failing on its own.
    let result =
        super::plugin_output::persist_plugin_outputs(&ctx.plugins, &ctx.config, target, &finalized_base, result).await;
    let outcome = if result.is_ok() { TargetAnalyticsOutcome::Published } else { TargetAnalyticsOutcome::Failed };
    persist_target_analytics(&config, &target.name, analytics_dto, &unmapped_csv, outcome).await;
    let watch_playlist = target_watch_view(&finalized_base, finalized_xtream.as_deref());
    if result.is_ok() && process_watch(&ctx.config, &ctx.events, target, watch_playlist).await {
        step.tick("group watches");
        log_memory_snapshot(format!("target '{}' after_group_watches", target.name).as_str());
    }
    step.stop("Persisting playlists");
    log_memory_snapshot(format!("target '{}' after_persist", target.name).as_str());
    (result, errors)
}

pub(crate) async fn playlist_resolve<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    pipe: &ProcessingPipe,
    provider_fpl: &mut FetchedPlaylist<'_>,
    processed_fpl: &mut FetchedPlaylist<'_>,
) -> PluginRunReport {
    // Resolve and probe talk to the provider; an input in maintenance keeps what it has.
    if ctx.is_input_in_maintenance(&processed_fpl.input.name) {
        return PluginRunReport::default();
    }
    let series_report = playlist_resolve_series(ctx, target, errors, pipe, provider_fpl, processed_fpl).await;
    playlist_resolve_vod(ctx, target, errors, provider_fpl, processed_fpl).await;
    playlist_probe(ctx, target, processed_fpl).await;
    series_report
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
pub(crate) async fn playlist_probe<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
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

#[derive(Debug)]
pub(crate) struct TargetPlaylistViews {
    pub(crate) base: Vec<PlaylistGroup>,
    pub(crate) xtream: Option<Vec<PlaylistGroup>>,
    pub(crate) publication_plan: PlaylistPublicationPlan,
    pub(crate) failover_groups: FailoverGroupBatch,
}

/// Builds the curated playlist views for base and Xtream outputs.
///
/// Note on curated Xtream categories: items copied into curated categories (or cloned
/// for the Xtream base catalog) are created after the pipeline re-apply stage and therefore
/// inherit any locked override values. Both `views.base` and `views.xtream` then pass through
/// subsequent finalization stages where locked overrides continue to be re-applied.
pub(crate) fn build_curated_playlist_views(
    playlist: Vec<PlaylistGroup>,
    evaluation: &CurationEvaluation,
    config: &CurationConfig,
    appearance_filter_configured: bool,
    has_xtream_output: bool,
) -> TargetPlaylistViews {
    let curated_catalog = config.catalog_selection.is_curated();
    let categories = has_xtream_output.then(|| project_curation_categories(evaluation, &playlist, config));
    let base = select_target_catalog(playlist, evaluation, curated_catalog);
    let xtream = categories.map(|mut categories| {
        let mut view = if config.include_xtream_base_categories { base.clone() } else { live_only(&base) };
        view.append(&mut categories);
        view
    });
    TargetPlaylistViews {
        base,
        xtream,
        publication_plan: PlaylistPublicationPlan::complete_curation_with_filter(
            curated_catalog,
            has_xtream_output && !config.include_xtream_base_categories,
            appearance_filter_configured,
        ),
        failover_groups: FailoverGroupBatch::default(),
    }
}

pub(crate) async fn prepare_target_playlist_views(
    client: &reqwest::Client,
    tmdb_client: Option<&reqwest::Client>,
    target: &ConfigTarget,
    playlist: Vec<PlaylistGroup>,
) -> TargetPlaylistViews {
    let Some(config) = target.effective_curation() else {
        return TargetPlaylistViews {
            base: playlist,
            xtream: None,
            publication_plan: PlaylistPublicationPlan::Ordinary,
            failover_groups: FailoverGroupBatch::default(),
        };
    };

    let outcome = evaluate_curation(client, tmdb_client, &playlist, &target.name, &config).await;
    curation_playlist_views(target, playlist, &config, outcome)
}

pub(super) fn curation_playlist_views(
    target: &ConfigTarget,
    playlist: Vec<PlaylistGroup>,
    config: &CurationConfig,
    outcome: CurationRunOutcome,
) -> TargetPlaylistViews {
    match outcome {
        CurationRunOutcome::NotConfigured => TargetPlaylistViews {
            base: playlist,
            xtream: None,
            publication_plan: PlaylistPublicationPlan::Ordinary,
            failover_groups: FailoverGroupBatch::default(),
        },
        CurationRunOutcome::Failed(failure) => {
            warn_curation_failure(&target.name, &failure);
            // Discovery is optional; unavailable selections must not suppress the source catalog or EPG.
            TargetPlaylistViews {
                base: playlist,
                xtream: None,
                publication_plan: PlaylistPublicationPlan::Ordinary,
                failover_groups: FailoverGroupBatch::default(),
            }
        }
        CurationRunOutcome::Complete(evaluation) => {
            let views = build_curated_playlist_views(
                playlist,
                &evaluation,
                config,
                target.filter.persist.is_some(),
                target.get_xtream_output().is_some(),
            );
            info!(
                "Target '{}' curation completed with {} memberships and {} Xtream groups",
                target.name,
                evaluation.memberships.len(),
                views.xtream.as_deref().map_or(0, <[PlaylistGroup]>::len)
            );
            views
        }
    }
}
fn warn_curation_failure(target_name: &str, failure: &CurationFailure) {
    let mut complete = 0usize;
    let mut incomplete = 0usize;
    let mut unavailable = 0usize;
    for outcome in &failure.selector_outcomes {
        match outcome {
            SelectorOutcome::Complete { .. } => complete += 1,
            SelectorOutcome::Incomplete { .. } => incomplete += 1,
            SelectorOutcome::Unavailable { .. } => unavailable += 1,
        }
    }
    warn!(
        "Target '{target_name}' curation refresh failed (complete selectors: {complete}, incomplete: {incomplete}, unavailable: {unavailable}); continuing with the regular catalog"
    );
}

fn item_subject_uuid(item: &PlaylistItem) -> UUIDType {
    if item.header.uuid == UUIDType::default() {
        item.get_uuid()
    } else {
        item.header.uuid
    }
}

fn selected_series_parent_keys(playlist: &[PlaylistGroup], selected_subjects: &HashSet<UUIDType>) -> HashSet<Arc<str>> {
    let mut parent_keys = HashSet::new();
    for item in playlist.iter().flat_map(|group| &group.channels) {
        if !item.header.item_type.is_series_info() || !selected_subjects.contains(&item_subject_uuid(item)) {
            continue;
        }
        parent_keys.insert(item.get_uuid().intern());
        if item.header.uuid != UUIDType::default() {
            parent_keys.insert(item.header.uuid.intern());
        }
        if item.header.virtual_id.get() != 0 {
            parent_keys.insert(item.header.virtual_id.get().to_string().intern());
        }
        if item.header.item_type == PlaylistItemType::LocalSeriesInfo && !item.header.id.is_empty() {
            parent_keys.insert(Arc::clone(&item.header.id));
        }
    }
    parent_keys
}

pub(crate) fn select_target_catalog(
    mut playlist: Vec<PlaylistGroup>,
    evaluation: &CurationEvaluation,
    enabled: bool,
) -> Vec<PlaylistGroup> {
    if !enabled {
        return playlist;
    }

    let selected_subjects =
        evaluation.memberships.iter().map(|membership| membership.subject_uuid).collect::<HashSet<_>>();
    let selected_parent_keys = selected_series_parent_keys(&playlist, &selected_subjects);

    for group in &mut playlist {
        group.channels.retain(|item| {
            if item.header.xtream_cluster == XtreamCluster::Live {
                return true;
            }
            match item.header.item_type {
                PlaylistItemType::Video
                | PlaylistItemType::LocalVideo
                | PlaylistItemType::SeriesInfo
                | PlaylistItemType::LocalSeriesInfo => selected_subjects.contains(&item_subject_uuid(item)),
                PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                    selected_parent_keys.contains(&item.header.parent_code)
                }
                _ => false,
            }
        });
    }
    playlist.retain(|group| group.xtream_cluster == XtreamCluster::Live || !group.channels.is_empty());
    playlist
}

fn live_only(playlist: &[PlaylistGroup]) -> Vec<PlaylistGroup> {
    playlist
        .iter()
        .filter_map(|group| {
            let mut live_group = group.clone();
            live_group.channels.retain(|item| item.header.xtream_cluster == XtreamCluster::Live);
            (group.xtream_cluster == XtreamCluster::Live || !live_group.channels.is_empty()).then_some(live_group)
        })
        .collect()
}

pub(crate) fn target_watch_view<'playlist>(
    base: &'playlist [PlaylistGroup],
    xtream: Option<&'playlist [PlaylistGroup]>,
) -> &'playlist [PlaylistGroup] {
    xtream.unwrap_or(base)
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

    // One snapshot for the whole refresh rather than a config load per group.
    let config = app_config.config.load_full();
    let storage_dir = config.storage_dir.as_str();

    // Before the per-group fan-out: this is about which groups exist, not
    // what is inside the ones the patterns name, so it must see every group
    // rather than only the watched ones.
    let group_changes = diff_target_groups(storage_dir, events, &target.name, new_playlist, watches).await;

    // Only `channels` entries are matched against the current groups: they
    // pick the groups to diff, and they are the only ones the unmatched
    // warning reports on (`ConfigWatch::reports_unmatched`). Once a group is
    // watched, an entry that already matched elsewhere needs no regex run.
    let mut matched = vec![false; watches.len()];
    let mut group_watches = Vec::new();
    for group in new_playlist {
        let mut watch_channels = false;
        for (index, watch) in watches.iter().enumerate() {
            if !watch.contexts.contains(WatchContext::Channels) || (watch_channels && matched[index]) {
                continue;
            }
            if watch.pattern.is_match(&group.title) {
                matched[index] = true;
                watch_channels = true;
            }
        }
        if watch_channels {
            // Futures built up front: a lazy `map` closure over borrowed state
            // trips the higher-ranked `Send` check of the spawned pipeline task.
            group_watches.push(process_group_watch(storage_dir, events, &target.name, group));
        }
    }

    // A channel pattern that matches nothing looks exactly like a group that
    // has not changed, so a typo must surface. Entries waiting for a group to
    // appear, and removal-only entries, match nothing by design.
    // `EventKindMask::from_wire_names` reports unmatched names for the same reason.
    let unmatched: Vec<String> = watches
        .iter()
        .zip(&matched)
        .filter(|(watch, matched)| !**matched && watch.reports_unmatched())
        .map(|(watch, _)| watch.pattern.as_str().to_string())
        .collect();
    if !unmatched.is_empty() {
        warn!("target '{}' has {} watch pattern(s) matching no group", target.name, unmatched.len());
        events.emit(EventMessage::PlaylistWatchUnmatched(WatchUnmatched::new(
            target.name.clone(),
            unmatched,
            new_playlist.len(),
        )));
    }

    let channel_diffs: Vec<_> =
        futures::stream::iter(group_watches).buffer_unordered(16).filter_map(futures::future::ready).collect().await;

    if let Some(changes) = assemble_watch_changes(&target.name, group_changes, channel_diffs) {
        events.emit(EventMessage::PlaylistWatchChanged(changes));
    }

    true
}

#[cfg(test)]
pub(crate) async fn run_finalize_until_persist(
    target: &ConfigTarget,
    playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Vec<tuliprox_repository::PendingFailoverGroup>) {
    run_finalize_until_persist_with_locked(target, playlist, HashMap::new()).await
}

#[cfg(test)]
pub(crate) async fn run_finalize_until_persist_with_locked(
    target: &ConfigTarget,
    mut playlist: Vec<PlaylistGroup>,
    mut locked: HashMap<UUIDType, Arc<ItemOverride>>,
) -> (Vec<PlaylistGroup>, Vec<tuliprox_repository::PendingFailoverGroup>) {
    let mut step = StepMeasure::new("test", |_, _| {});
    let pending = if let Some(target_cfg) = target.execution_plan.failover_groups.as_ref() {
        let cg_cfg = shared::model::FailoverGroupsConfig::default().for_target(target_cfg);
        crate::processor::failover_group::group_playlist(&cg_cfg, None, &target.name, &mut playlist).groups
    } else {
        Vec::new()
    };
    lock_group_representatives(&pending, &mut locked);
    if target.favourites.is_some() {
        process_favourites(&mut playlist, target.favourites.as_deref());
    }
    let group_uuids: HashSet<UUIDType> = pending.iter().map(|p| p.group_uuid).collect();
    let mut report = PluginRunReport::default();
    let eligible = prepare_eligible_catalog(&NoPlugins, target, playlist, &group_uuids, &mut step, &mut report)
        .await
        .expect("no plugin to abort");
    let finalized = finalize_playlist_view(
        &NoPlugins,
        target,
        eligible,
        &mut step,
        PlaylistViewKind::Base,
        &mut PluginChnos::default(),
        &mut report,
        Some(&locked),
    )
    .await
    .expect("no plugin to abort");
    (finalized, pending)
}

/// Reconcile each binding once for the entire target, then preserve its per-input playback origin.
pub(crate) async fn reconcile_target_managed<P: PluginHost>(
    plugins: &P,
    target: &ConfigTarget,
    inputs: &[&tuliprox_core::model::ConfigInput],
    bouquet_filter: Option<&tuliprox_core::model::TargetBouquetFilter>,
    report: &mut PluginRunReport,
) -> Result<HashMap<Arc<str>, ManagedStageItems>, PluginAbort> {
    let allowed_inputs: Vec<String> = inputs
        .iter()
        .filter(|input| !super::ingest::collect_effective_skip_clusters(input).contains(&XtreamCluster::Live))
        .map(|input| input.name.to_string())
        .collect();
    let mut managed: HashMap<Arc<str>, ManagedStageItems> = HashMap::new();
    for binding in &target.managed_channels {
        let request = shared::model::ManagedChannelRequest {
            destination: shared::model::ManagedDestination::Target,
            name: &target.name,
            stage: Some(binding.stage),
            inputs: &allowed_inputs,
        };
        let contribution = plugins.managed_channels(binding.plugin.id(), &request, report).await?;
        for mut item in contribution {
            if !allowed_inputs.iter().any(|input| input.as_str() == &*item.header.input_name) {
                return Err(PluginAbort {
                    plugin: binding.plugin.id().intern(),
                    hook: PluginHook::ManagedChannels,
                    reason: "managed channel source input is not available to this target".to_string(),
                });
            }
            if bouquet_filter
                .as_ref()
                .is_some_and(|filter| !filter.allows(item.header.xtream_cluster, &item.header.group))
            {
                continue;
            }
            item.header.source_ordinal = u32::MAX;
            let stages = managed.entry(Arc::clone(&item.header.input_name)).or_default();
            match binding.stage {
                shared::model::ManagedChannelStage::BeforeFilter => stages.before.push(item),
                shared::model::ManagedChannelStage::AfterFilter => stages.after.push(item),
            }
        }
    }
    Ok(managed)
}

#[cfg(test)]
mod filter_plugin_tests {
    use super::*;
    use shared::model::NoPlugins;
    use tuliprox_core::model::ConfigTarget;

    fn target_with_filter(filter: &str) -> ConfigTarget {
        let mut dto: shared::model::ConfigTargetDto =
            serde_json::from_value(serde_json::json!({"name": "t1"})).expect("target fixture");
        dto.filter.t_processing =
            Some(shared::foundation::get_target_processing_filter(filter, None).expect("filter compiles"));
        ConfigTarget::from(&dto)
    }

    /// A filter term whose plugin is gone must not empty the target behind a `success`: the target
    /// fails, keeps its previous output, and the message names the plugin.
    #[test]
    fn a_filter_term_without_its_plugin_fails_the_target() {
        let target = target_with_filter(r#"Group ~ ".*" and plugin.youtube-input.is_live("News")"#);

        let err = ensure_filter_plugins_available(&NoPlugins, &target).expect_err("the target cannot run");

        let message = err.to_string();
        assert!(message.contains("target 't1' is inactive"), "{message}");
        assert!(message.contains("youtube-input"), "{message}");
    }

    #[test]
    fn a_filter_without_plugin_terms_runs() {
        let target = target_with_filter(r#"Group ~ "News""#);
        assert!(ensure_filter_plugins_available(&NoPlugins, &target).is_ok());
    }
}
