#![allow(clippy::wildcard_imports)]
use super::{
    fetch_outcome::{apply_playlist_fetch_outcome, CacheStatusScope},
    *,
};

// Inputs disabled in the config are always disabled.
// Command-line targets can only restrict enabled inputs, never enable them.
pub(crate) fn is_input_enabled(input: &ConfigInput, user_targets: &ProcessTargets) -> bool {
    input.enabled && (!user_targets.enabled || user_targets.has_input(input.id))
}

pub(crate) async fn with_sequential_group<T>(
    file_locks: &tuliprox_core::utils::FileLockManager,
    group: Option<u32>,
    process_parallel: bool,
    future: impl std::future::Future<Output = T>,
) -> T {
    let _guard = if process_parallel {
        if let Some(group) = group {
            Some(file_locks.write_lock_str(&format!("sequential_group:{group}")).await)
        } else {
            None
        }
    } else {
        None
    };
    future.await
}

pub(crate) struct PlaylistDownloadResult {
    pub downloaded_playlist: Vec<PlaylistGroup>,
    pub download_err: Vec<TuliproxError>,
    pub quality_rejections: Vec<ClusterUpdateRejection>,
    pub was_cached: bool,
    pub persisted: bool,
    pub partial: bool,
}

impl PlaylistDownloadResult {
    pub fn new(
        downloaded_playlist: Vec<PlaylistGroup>,
        download_err: Vec<TuliproxError>,
        was_cached: bool,
        persisted: bool,
    ) -> Self {
        Self {
            downloaded_playlist,
            download_err,
            quality_rejections: Vec::new(),
            was_cached,
            persisted,
            partial: false,
        }
    }
}

impl From<PlaylistFetch> for PlaylistDownloadResult {
    fn from(fetch: PlaylistFetch) -> Self {
        Self {
            downloaded_playlist: fetch.groups,
            download_err: fetch.errors,
            quality_rejections: fetch.quality_rejections,
            was_cached: false,
            persisted: fetch.persisted,
            partial: fetch.partial,
        }
    }
}

pub(crate) fn collect_effective_skip_clusters(input: &ConfigInput) -> Vec<XtreamCluster> {
    if !input.input_type.is_xtream() {
        return vec![];
    }
    xtream::get_skip_cluster(input)
}

pub(crate) fn filter_skipped_clusters_from_source(source: PlaylistSource, input: &ConfigInput) -> PlaylistSource {
    let skip_clusters = collect_effective_skip_clusters(input);
    if skip_clusters.is_empty() {
        return source;
    }

    let skip_set: HashSet<XtreamCluster> = skip_clusters.into_iter().collect();
    PlaylistSource::filtered(source, skip_set)
}

pub(crate) fn cluster_selected(cluster: XtreamCluster, clusters: ClusterFlags) -> bool {
    match cluster {
        XtreamCluster::Live => clusters.contains(ClusterFlags::Live),
        XtreamCluster::Video => clusters.contains(ClusterFlags::Vod),
        XtreamCluster::Series => clusters.contains(ClusterFlags::Series),
    }
}

pub(crate) fn apply_staged_overlay_groups(
    provider_name: &Arc<str>,
    clusters: ClusterFlags,
    provider_groups: Vec<PlaylistGroup>,
    staged_groups: Vec<PlaylistGroup>,
) -> Vec<PlaylistGroup> {
    let mut groups: Vec<PlaylistGroup> =
        provider_groups.into_iter().filter(|group| !cluster_selected(group.xtream_cluster, clusters)).collect();

    groups.extend(staged_groups.into_iter().filter(|group| cluster_selected(group.xtream_cluster, clusters)).map(
        |mut group| {
            for item in &mut group.channels {
                item.header.input_name = Arc::clone(provider_name);
            }
            group
        },
    ));

    groups
}

pub(crate) fn should_apply_staged_overlay(download_result: &PlaylistDownloadResult) -> bool {
    !download_result.was_cached
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn playlist_download_from_input<E: EventSink>(
    client: &reqwest::Client,
    app_config: &Arc<AppConfig>,
    events: &E,
    input: &ConfigInput,
    stalker_refresh_mode: StalkerRefreshMode,
) -> PlaylistDownloadResult {
    let config = &*app_config.config.load();
    let storage_dir = &config.storage_dir;

    // Check Status
    let storage_path = input_cache::resolve_input_storage_path(storage_dir, &input.name).await;
    let mut status = input_cache::load_input_status(&storage_path);
    let cache_duration = input.cache_duration_seconds;

    // Ensure data directory exists
    match tokio::fs::try_exists(&storage_path).await {
        Ok(false) => {
            if let Err(err) = tokio::fs::create_dir_all(&storage_path).await {
                warn!("Failed to create input storage directory '{}': {err}", storage_path.display());
            }
        }
        Err(err) => {
            warn!("Failed to check existence of input storage directory '{}': {err}", storage_path.display());
        }
        Ok(true) => {}
    }

    let download_input_type = input.get_download_input_type();
    // Use per-cluster cache for effective Xtream downloads.
    let use_per_cluster_cache = download_input_type.is_xtream();

    let mut xtream_clusters_to_download = Vec::new();
    let fully_cached = if use_per_cluster_cache {
        let skip_cluster = collect_effective_skip_clusters(input);
        let xtream_cache_candidates = xtream::requested_clusters(None, &skip_cluster);

        for cluster in xtream_cache_candidates {
            if !input_cache::is_cache_valid(&status, cluster.as_ref(), cache_duration) {
                xtream_clusters_to_download.push(cluster);
            }
        }

        xtream_clusters_to_download.is_empty()
    } else {
        input_cache::is_cache_valid(&status, "default", cache_duration)
    };

    if fully_cached {
        return PlaylistDownloadResult::new(vec![], vec![], true, false);
    }

    let request = PlaylistFetchRequest {
        app_config,
        config: &app_config.config.load(),
        client,
        input,
        xtream_clusters: Some(xtream_clusters_to_download.as_slice()),
    };

    // Each arm builds the provider its input type needs and awaits it in place: the
    // provider types share no supertype, and building one is free, so this stays a match
    // and stays statically dispatched. What changed is the result - one named
    // `PlaylistFetch` instead of a six-element tuple assembled by position.
    let fetch = match download_input_type {
        InputType::M3u => M3uProvider.fetch(&request).await,
        InputType::Xtream => XtreamProvider::new(events).fetch(&request).await,
        InputType::M3uBatch | InputType::XtreamBatch | InputType::StalkerBatch => {
            BatchContainerProvider.fetch(&request).await
        }
        InputType::Stalker => {
            StalkerProvider::new(stalker_refresh_mode, !config.disk_based_processing).fetch(&request).await
        }
        InputType::Library => LibraryProvider.fetch(&request).await,
        InputType::Plex => PlexProvider.fetch(&request).await,
        InputType::Emby | InputType::Jellyfin => {
            UnsupportedProvider::new(
                "media-server",
                format!("media-server input '{}' is configured but catalog import is not implemented yet", input.name),
            )
            .fetch(&request)
            .await
        }
        InputType::Staged => {
            UnsupportedProvider::new(
                "staged",
                format!("staged input '{}' was not resolved against a parent input", input.name),
            )
            .fetch(&request)
            .await
        }
    };
    // `ProviderErrorKind` has always been able to answer "is this worth
    // retrying, and does it need a human" - `needs_operator()` is exactly that
    // question - and nothing consumed the answer. Every fetch failure was
    // counted, logged and treated identically.
    if let Some(kind) = fetch.error_kind() {
        let worst = fetch
            .errors
            .iter()
            .max_by_key(|error| ProviderErrorKind::of_tuliprox(error))
            .map(|error| sanitize_sensitive_info(&error.to_string()).into_owned());
        events.emit(EventMessage::ProviderFetchFailed(ProviderFetchFailure {
            input: sanitize_sensitive_info(&input.name).into_owned().into(),
            provider: download_input_type.to_string().into(),
            kind: kind.into(),
            error_count: fetch.errors.len(),
            message: worst,
            retryable: kind.is_retryable(),
            needs_operator: kind.needs_operator(),
            partial: fetch.partial,
        }));
    }

    let cache_scope = if use_per_cluster_cache {
        CacheStatusScope::RequestedClusters(&xtream_clusters_to_download)
    } else {
        CacheStatusScope::Default
    };
    let save_status = apply_playlist_fetch_outcome(events, input, &mut status, cache_scope, &fetch);

    if save_status {
        input_cache::save_input_status(&storage_path, &status);
    }

    PlaylistDownloadResult::from(fetch)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputJobState {
    Ready,
    Pending,
    Failed,
}

pub(crate) struct InputDownloadResult {
    pub(crate) errors: Vec<TuliproxError>,
    pub(crate) source: PlaylistSource,
    pub(crate) storage_error: Option<TuliproxError>,
    pub(crate) partial: bool,
    pub(crate) quality_rejections: Vec<ClusterUpdateRejection>,
}

impl InputDownloadResult {
    pub(crate) fn job_state(&mut self) -> InputJobState {
        if self.partial {
            InputJobState::Pending
        } else if self.storage_error.is_some() || self.source.is_empty() {
            InputJobState::Failed
        } else {
            InputJobState::Ready
        }
    }
}

pub(crate) struct InputJobResult {
    pub(crate) index: usize,
    pub(crate) input_name: Arc<str>,
    pub(crate) state: InputJobState,
    pub(crate) source: Option<PlaylistSource>,
    pub(crate) epg: Option<TVGuide>,
    pub(crate) stat: InputStats,
    pub(crate) errors: Vec<TuliproxError>,
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
    let broadcast_step = create_broadcast_callback(&ctx.events);
    broadcast_step("Playlist download", &format!("Downloading input '{}'", input.name));

    let mut download = download_input(ctx, input, false).await;
    let state = download.job_state();
    let storage_failed = download.storage_error.is_some();
    if !download.quality_rejections.is_empty() {
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
    if state == InputJobState::Failed && download.source.is_empty() {
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
        input_name: input.name.clone(),
        state,
        source: (state == InputJobState::Ready).then_some(download.source),
        epg,
        stat,
        errors: download.errors,
    }
}

pub(crate) fn panicked_input_job(index: usize, input: &ConfigInput) -> InputJobResult {
    let error = TuliproxError::RepositoryPlaylist(format!("Input '{}' processing panicked", input.name));
    InputJobResult {
        index,
        input_name: input.name.clone(),
        state: InputJobState::Failed,
        source: None,
        epg: None,
        stat: create_input_stat(0, 0, 1, input.get_download_input_type(), &input.name, 0),
        errors: vec![error],
    }
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
        let broadcast_step = create_broadcast_callback(&ctx.events);
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
        for mut result in job_results {
            errors.append(&mut result.errors);
            input_stats.insert(result.input_name.clone(), result.stat);
            if result.state == InputJobState::Ready {
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

/// `invalidate_input_cache_status` performs a non-atomic file I/O sequence
/// (`input_cache::load_input_status` + `input_cache::save_input_status`).
/// Call this only while holding the per-input lock from
/// `PlaylistProcessingContext::get_input_lock` (as done in `download_input`).
pub(crate) async fn invalidate_input_cache_status<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    input: &ConfigInput,
) {
    let storage_dir = { ctx.config.config.load().storage_dir.clone() };
    let storage_path = input_cache::resolve_input_storage_path(&storage_dir, &input.name).await;
    let mut status = input_cache::load_input_status(&storage_path);
    if !status.clusters.is_empty() {
        status.clusters.clear();
        input_cache::save_input_status(&storage_path, &status);
    }
}

pub(crate) async fn load_cached_input_playlist<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    input: &Arc<ConfigInput>,
) -> (PlaylistSource, Option<TuliproxError>) {
    match load_input_playlist(&ctx.config, input, None).await {
        Ok(pl_source) => (pl_source, None),
        Err(err) => (MemoryPlaylistSource::default().into_source(), Some(err)),
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn download_input<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    input: &Arc<ConfigInput>,
    allow_staged_input: bool,
) -> InputDownloadResult {
    if input.staged.is_some() && !allow_staged_input {
        return InputDownloadResult {
            errors: Vec::new(),
            source: MemoryPlaylistSource::default().into_source(),
            storage_error: None,
            partial: false,
            quality_rejections: Vec::new(),
        };
    }

    let staged_overlay = if input.staged.is_none() {
        let sources = ctx.config.sources.load();
        sources.get_staged_input_for_provider(&input.name).cloned()
    } else {
        None
    };

    // Coordination Logic
    let need_download = !ctx.is_input_downloaded(&input.name).await;
    // Keep this lock for the whole critical section (download + persist/load + mark processed)
    // so parallel sources sharing the same input cannot observe a half-written state.
    let mut input_lock = if need_download { Some(ctx.get_input_lock(&input.name).await) } else { None };
    let mut mark_as_processed = false;

    let mut playlist_download_result = if need_download {
        // Check again after lock
        let already_processed = ctx.is_input_downloaded(&input.name).await;

        if already_processed {
            // Use empty results, will load from disk below
            PlaylistDownloadResult::new(vec![], vec![], true, false)
        } else if ctx.pre_processed_inputs.as_ref().is_some_and(|s| s.contains(&input.name)) {
            // Input was already processed in a prior session; skip download and load from disk.
            // Mark only after load succeeds (or fails) to avoid exposing a half-ready state.
            mark_as_processed = true;
            PlaylistDownloadResult::new(vec![], vec![], true, false)
        } else {
            mark_as_processed = true;
            playlist_download_from_input(&ctx.client, &ctx.config, &ctx.events, input, ctx.stalker_refresh_mode).await
        }
    } else {
        PlaylistDownloadResult::new(vec![], vec![], true, false)
    };

    let mut preloaded_playlist: Option<(PlaylistSource, Option<TuliproxError>)> = None;
    if playlist_download_result.was_cached {
        let (cached_playlist, cached_error) = load_cached_input_playlist(ctx, input).await;
        // Defensive fallback: if cache metadata says "valid" but persisted data is unreadable,
        // retry once before forcing a refresh.
        let must_force_refresh = cached_error.is_some();
        if must_force_refresh {
            warn!("Input '{}' cache hit produced unreadable playlist; retrying cached load once", input.name);
            let (retry_playlist, retry_error) = load_cached_input_playlist(ctx, input).await;
            if retry_error.is_none() {
                preloaded_playlist = Some((retry_playlist, None));
            } else {
                if input_lock.is_none() {
                    input_lock = Some(ctx.get_input_lock(&input.name).await);
                }
                // Re-check immediately after locking to avoid duplicate refreshes when another worker
                // repaired the cache between our earlier retry and lock acquisition.
                let (locked_retry_playlist, locked_retry_error) = load_cached_input_playlist(ctx, input).await;
                if locked_retry_error.is_none() {
                    warn!("Input '{}' cache became readable after lock re-check; skipping refresh", input.name);
                    preloaded_playlist = Some((locked_retry_playlist, None));
                } else {
                    warn!(
                        "Input '{}' cached playlist remained unreadable after retry and lock re-check; invalidating cache and forcing refresh",
                        input.name
                    );
                    invalidate_input_cache_status(ctx, input).await;
                    playlist_download_result = playlist_download_from_input(
                        &ctx.client,
                        &ctx.config,
                        &ctx.events,
                        input,
                        ctx.stalker_refresh_mode,
                    )
                    .await;
                }
            }
        } else {
            preloaded_playlist = Some((cached_playlist, None));
        }
    }
    if playlist_download_result.partial {
        ctx.partial_refresh.store(true, std::sync::atomic::Ordering::Release);
        ctx.events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
            target: input.name.to_string(),
            message: stalker_checkpoint_message(&input.name),
        }));
    }
    let apply_staged_overlay = should_apply_staged_overlay(&playlist_download_result);
    let reuse_persisted_after_quality_rejection = !playlist_download_result.quality_rejections.is_empty()
        && playlist_download_result.downloaded_playlist.is_empty()
        && !playlist_download_result.persisted;

    let (mut playlist, mut error) = if let Some(preloaded) = preloaded_playlist {
        preloaded
    } else if playlist_download_result.was_cached
        || playlist_download_result.persisted
        || reuse_persisted_after_quality_rejection
    {
        match load_input_playlist(&ctx.config, input, None).await {
            Ok(pl_source) => (pl_source, None),
            Err(e) => (MemoryPlaylistSource::default().into_source(), Some(e)),
        }
    } else {
        debug!("Persisting input '{}' playlist", input.name);
        let (pl, err) = persist_input_playlist(&ctx.config, input, playlist_download_result.downloaded_playlist).await;
        (MemoryPlaylistSource::new(pl).into_source(), err)
    };

    playlist = filter_skipped_clusters_from_source(playlist, input);

    if let Some(staged_input) = staged_overlay.filter(|_| apply_staged_overlay) {
        let clusters = staged_input.staged.as_ref().map_or_else(ClusterFlags::all, |staged| staged.clusters);
        let mut staged_result = Box::pin(download_input(ctx, &staged_input, true)).await;
        playlist_download_result.partial |= staged_result.partial;
        playlist_download_result.download_err.append(&mut staged_result.errors);
        playlist_download_result.quality_rejections.append(&mut staged_result.quality_rejections);
        if let Some(staged_error) = staged_result.storage_error {
            playlist_download_result.download_err.push(staged_error);
        } else {
            let provider_groups = playlist.take_groups();
            let staged_groups = staged_result.source.take_groups();
            let merged_groups = apply_staged_overlay_groups(&input.name, clusters, provider_groups, staged_groups);
            let (merged_playlist, persist_error) = persist_input_playlist(&ctx.config, input, merged_groups).await;
            playlist = MemoryPlaylistSource::new(merged_playlist).into_source();
            if error.is_none() {
                error = persist_error;
            } else if let Some(persist_error) = persist_error {
                playlist_download_result.download_err.push(persist_error);
            }
        }
    }

    if input.input_type == InputType::M3u {
        let alias_errors = download_m3u_alias_playlists(ctx, input).await;
        playlist_download_result.download_err.extend(alias_errors);
    }

    if mark_as_processed && !playlist_download_result.partial && error.is_none() && !playlist.is_empty() {
        // Mark after persist/load so other workers only see this input as ready when data is usable.
        ctx.mark_input_downloaded(input.name.clone()).await;
    }

    // Explicitly release per-input lock after load/persist/mark steps are completed.
    drop(input_lock);

    InputDownloadResult {
        errors: playlist_download_result.download_err,
        source: playlist,
        storage_error: error,
        partial: playlist_download_result.partial,
        quality_rejections: playlist_download_result.quality_rejections,
    }
}

async fn download_m3u_alias_playlists<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    input: &ConfigInput,
) -> Vec<TuliproxError> {
    let Some(aliases) = input.get_enabled_aliases() else { return vec![] };
    let mut errors = Vec::new();

    for alias in aliases {
        if ctx.is_input_downloaded(&alias.name).await {
            continue;
        }

        let mut alias_input = input.as_input(alias);
        // A user-provided raw-playlist persist path belongs to the primary input. Alias
        // snapshots use their own internal storage so accounts never overwrite each other.
        alias_input.persist = None;
        alias_input.epg = None;
        let alias_input = Arc::new(alias_input);

        let mut alias_result = Box::pin(download_input(ctx, &alias_input, false)).await;
        let alias_had_errors = !alias_result.errors.is_empty() || alias_result.storage_error.is_some();
        errors.append(&mut alias_result.errors);
        if let Some(storage_error) = alias_result.storage_error {
            errors.push(storage_error);
        }
        if alias_result.partial {
            errors.push(TuliproxError::RepositoryPlaylist(format!(
                "M3U alias '{}' returned a partial playlist",
                alias.name
            )));
        } else if alias_result.source.is_empty() && !alias_had_errors {
            errors.push(TuliproxError::RepositoryPlaylist(format!("M3U alias '{}' playlist is empty", alias.name)));
        }
    }

    errors
}

pub(crate) fn create_broadcast_callback<E: EventSink + Clone + 'static>(events: &E) -> StepMeasureCallback {
    let events = events.clone();
    Box::new(move |context: &str, msg: &str| {
        events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
            target: context.to_owned(),
            message: msg.to_owned(),
        }));
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

pub struct PlaylistProcessingContext<E: EventSink, M: MetadataUpdateSink = NoopMetadataSink> {
    pub client: reqwest::Client,
    pub config: Arc<AppConfig>,
    pub user_targets: Arc<ProcessTargets>,
    pub events: E,
    pub playlist_state: Option<Arc<PlaylistStorageState>>,
    /// Reverse-proxy header suppression, carried from the composition root.
    ///
    /// Nothing in the pipeline reads this today. It became visible when
    /// `load_input_playlist` stopped taking the whole context, and it is left in
    /// place rather than deleted because the plumbing exists in the API layer
    /// and in `exec_processing`'s signature: a configured value that is accepted
    /// and ignored is a behaviour question, not a refactoring one.
    #[allow(dead_code)]
    pub disabled_headers: Option<ReverseProxyDisabledHeaderConfig>,

    // Coordination
    pub processed_inputs: Arc<Mutex<HashSet<Arc<str>>>>,
    #[allow(clippy::type_complexity)]
    pub input_locks: Arc<Mutex<HashMap<Arc<str>, Weak<RwLock<()>>>>>,

    // New field for STRM probes & background updates
    pub provider_manager: Option<Arc<ActiveProviderManager>>,
    pub metadata_manager: Option<Arc<M>>,
    pub pre_processed_inputs: Option<Arc<HashSet<Arc<str>>>>,
    pub stalker_refresh_mode: StalkerRefreshMode,
    /// Resumable Stalker work that must remain `Pending` at input level.
    pub partial_refresh: Arc<std::sync::atomic::AtomicBool>,
    /// Completed, nonfatal quality decisions that make only the overall run partial.
    pub had_quality_rejections: Arc<std::sync::atomic::AtomicBool>,
}

// Written out rather than derived: `#[derive(Clone)]` would demand `M: Clone`,
// but the sink is held behind an `Arc` and is cloneable whatever `M` is.
impl<E: EventSink + Clone, M: MetadataUpdateSink> Clone for PlaylistProcessingContext<E, M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            config: Arc::clone(&self.config),
            user_targets: Arc::clone(&self.user_targets),
            events: self.events.clone(),
            playlist_state: self.playlist_state.clone(),
            disabled_headers: self.disabled_headers.clone(),
            processed_inputs: Arc::clone(&self.processed_inputs),
            input_locks: Arc::clone(&self.input_locks),
            provider_manager: self.provider_manager.clone(),
            metadata_manager: self.metadata_manager.clone(),
            pre_processed_inputs: self.pre_processed_inputs.clone(),
            stalker_refresh_mode: self.stalker_refresh_mode,
            partial_refresh: Arc::clone(&self.partial_refresh),
            had_quality_rejections: Arc::clone(&self.had_quality_rejections),
        }
    }
}

impl<E: EventSink + Clone + 'static, M: MetadataUpdateSink> PlaylistProcessingContext<E, M> {
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
