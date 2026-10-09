use super::{
    apply_playlist_fetch_outcome, apply_staged_overlay_groups, cluster_is_configured, collect_effective_skip_clusters,
    filter_skipped_clusters_from_source, neutralize_overlaid_cluster_facts, report_force_updates,
    should_apply_staged_overlay, stalker_checkpoint_message,
    status::{
        cached_playlist_download_result, forced_empty_cluster_flags, in_run_reuse_playlist_download_result,
        report_forced_update_request, ClusterStatusSource,
    },
    telemetry::{
        finalize_input_telemetry, input_telemetry_for_fetch, is_cluster_input, persist_finalized_cluster_snapshots,
    },
    CacheStatusScope, InputDownloadResult, LibraryProvider, PendingInputStatusUpdate, PlaylistDownloadResult,
    PlaylistProcessingContext, PlaylistUpdateExecutionRef, PlexProvider, StalkerProvider,
    PIPELINE_TRANSPARENCY_CLUSTERS,
};
use crate::{input_cache, metadata_sink::MetadataUpdateSink, processor::StalkerRefreshMode};
use log::{debug, warn};
use shared::{
    error::TuliproxError,
    model::{
        ClusterFlags, EventMessage, EventSink, InputRefreshPolicy, InputType, PlaylistUpdateDataSource,
        PlaylistUpdateProgressEvent, ProviderFetchFailure,
    },
    utils::sanitize_sensitive_info,
};
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, ConfigInput};
use tuliprox_iptv::{
    error::ProviderErrorKind,
    provider::{
        BatchContainerProvider, M3uProvider, PlaylistFetchRequest, PlaylistProvider, UnsupportedProvider,
        XtreamProvider,
    },
    xtream,
};
use tuliprox_repository::{
    load_input_playlist, persist_input_playlist_with_options, InputPlaylistPersistOptions, MemoryPlaylistSource,
    PlaylistSource,
};

/// Local discovery is not a provider Refresh policy. Both can require a fresh read.
#[derive(Clone, Copy)]
pub(super) enum InputAcquisition {
    Provider(InputRefreshPolicy),
    RescannedLibrary,
}

impl InputAcquisition {
    pub(super) fn refresh_policy(self) -> InputRefreshPolicy {
        match self {
            Self::Provider(policy) => policy,
            Self::RescannedLibrary => InputRefreshPolicy::NORMAL,
        }
    }

    pub(super) fn bypasses_cache(self) -> bool {
        match self {
            Self::Provider(policy) => policy.bypasses_cache(),
            Self::RescannedLibrary => true,
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn playlist_download_from_input<E: EventSink>(
    client: &reqwest::Client,
    app_config: &Arc<AppConfig>,
    events: &E,
    execution: PlaylistUpdateExecutionRef<'_>,
    input: &ConfigInput,
    stalker_refresh_mode: StalkerRefreshMode,
    acquisition: InputAcquisition,
) -> PlaylistDownloadResult {
    let refresh_policy = acquisition.refresh_policy();
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
            if acquisition.bypasses_cache() || !input_cache::is_cache_valid(&status, cluster.as_ref(), cache_duration) {
                xtream_clusters_to_download.push(cluster);
            }
        }

        xtream_clusters_to_download.is_empty()
    } else {
        !acquisition.bypasses_cache() && input_cache::is_cache_valid(&status, "default", cache_duration)
    };
    let cluster_status_source = if use_per_cluster_cache || download_input_type == InputType::M3u {
        ClusterStatusSource::PerCluster
    } else {
        ClusterStatusSource::Default
    };

    if fully_cached {
        return cached_playlist_download_result(input, refresh_policy).with_pending_status(PendingInputStatusUpdate {
            storage_path,
            status,
            status_changed: false,
            cluster_status_source,
        });
    }

    let provider_clusters = if is_cluster_input(download_input_type) {
        if use_per_cluster_cache {
            xtream_clusters_to_download.clone()
        } else {
            PIPELINE_TRANSPARENCY_CLUSTERS
                .into_iter()
                .filter(|cluster| cluster_is_configured(input, *cluster))
                .collect()
        }
    } else {
        Vec::new()
    };

    let request = PlaylistFetchRequest {
        app_config,
        config: &app_config.config.load(),
        client,
        input,
        xtream_clusters: Some(xtream_clusters_to_download.as_slice()),
        update_quality: refresh_policy.quality,
    };

    // Each arm builds the provider its input type needs and awaits it in place: the
    // provider types share no supertype, and building one is free, so this stays a match
    // and stays statically dispatched. What changed is the result - one named
    // `PlaylistFetch` instead of a six-element tuple assembled by position.
    let fetch = match download_input_type {
        InputType::M3u => {
            super::super::m3u_quality::effective_m3u_fetch(
                app_config,
                input,
                refresh_policy.quality,
                M3uProvider.fetch(&request).await,
            )
            .await
        }
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
    let save_status = apply_playlist_fetch_outcome(
        events,
        execution.run_id,
        execution.execution_order,
        input,
        &mut status,
        cache_scope,
        &fetch,
    );

    let input_telemetry = input_telemetry_for_fetch(
        input,
        refresh_policy,
        PlaylistUpdateDataSource::Provider,
        &provider_clusters,
        Some(&fetch),
    );
    PlaylistDownloadResult::from(fetch).with_input_telemetry(input_telemetry).with_pending_status(
        PendingInputStatusUpdate { storage_path, status, status_changed: save_status, cluster_status_source },
    )
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
            authoritative_library: false,
            errors: Vec::new(),
            source: MemoryPlaylistSource::default().into_source(),
            storage_error: None,
            partial: false,
            quality_rejections: Vec::new(),
            accepted_empty_clusters: ClusterFlags::empty(),
            input_telemetry: None,
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
    let refresh_policy = ctx.refresh_policy(input.id);

    let mut playlist_download_result = if need_download {
        // Check again after lock
        let already_processed = ctx.is_input_downloaded(&input.name).await;

        if already_processed {
            // Use empty results, will load from disk below
            in_run_reuse_playlist_download_result()
        } else if ctx.pre_processed_inputs.as_ref().is_some_and(|s| s.contains(&input.name)) {
            // Input was already processed in a prior session; skip download and load from disk.
            // Mark only after load succeeds (or fails) to avoid exposing a half-ready state.
            mark_as_processed = true;
            cached_playlist_download_result(input, refresh_policy)
        } else {
            mark_as_processed = true;
            report_forced_update_request(ctx, input, refresh_policy);
            playlist_download_from_input(
                &ctx.client,
                &ctx.config,
                &ctx.events,
                PlaylistUpdateExecutionRef { run_id: &ctx.run_id, execution_order: ctx.execution_order },
                input,
                ctx.stalker_refresh_mode,
                ctx.acquisition(input.id),
            )
            .await
        }
    } else {
        in_run_reuse_playlist_download_result()
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
                        PlaylistUpdateExecutionRef { run_id: &ctx.run_id, execution_order: ctx.execution_order },
                        input,
                        ctx.stalker_refresh_mode,
                        ctx.acquisition(input.id),
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
        ctx.events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::for_run_input(
            ctx.run_id.clone(),
            ctx.execution_order,
            input.id,
            input.name.to_string(),
            stalker_checkpoint_message(&input.name),
        )));
    }
    let apply_staged_overlay = should_apply_staged_overlay(&playlist_download_result);
    let authoritative_library = input.input_type == InputType::Library
        && playlist_download_result.download_err.is_empty()
        && !playlist_download_result.partial;
    let mut accepted_empty_clusters = forced_empty_cluster_flags(&playlist_download_result.force_updates);
    let reuse_persisted_after_quality_rejection = !playlist_download_result.quality_rejections.is_empty()
        && playlist_download_result.downloaded_playlist.is_empty()
        && !playlist_download_result.persisted;

    let (mut playlist, mut error) = if input.input_type == InputType::Library && !authoritative_library {
        // A failed catalog read must not replace an existing Library input with an empty tree.
        (MemoryPlaylistSource::default().into_source(), None)
    } else if let Some(preloaded) = preloaded_playlist {
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
        let (pl, err) = persist_input_playlist_with_options(
            &ctx.config,
            input,
            std::mem::take(&mut playlist_download_result.downloaded_playlist),
            InputPlaylistPersistOptions { accepted_empty_clusters },
        )
        .await;
        (MemoryPlaylistSource::new(pl).into_source(), err)
    };

    playlist = filter_skipped_clusters_from_source(playlist, input);

    if let Some(staged_input) = staged_overlay.filter(|_| apply_staged_overlay) {
        let clusters = staged_input.staged.as_ref().map_or_else(ClusterFlags::all, |staged| staged.clusters);
        let mut staged_result = Box::pin(download_input(ctx, &staged_input, true)).await;
        // download_input has released the staged input's lock. Record its own
        // facts before merging them into the parent's outcome or consuming groups.
        super::super::input_status::complete_staged_input(ctx, &staged_input, &mut staged_result).await;
        playlist_download_result.partial |= staged_result.partial;
        playlist_download_result.download_err.append(&mut staged_result.errors);
        playlist_download_result.quality_rejections.append(&mut staged_result.quality_rejections);
        accepted_empty_clusters |= staged_result.accepted_empty_clusters;
        if let Some(staged_error) = staged_result.storage_error {
            playlist_download_result.download_err.push(staged_error);
        } else {
            let provider_groups = playlist.take_groups();
            let staged_groups = staged_result.source.take_groups();
            let merged_groups =
                apply_staged_overlay_groups(input, staged_input.staged_type, clusters, provider_groups, staged_groups);
            if let Some(input_telemetry) = playlist_download_result.input_telemetry.as_mut() {
                neutralize_overlaid_cluster_facts(input_telemetry, clusters);
            }
            let (merged_playlist, persist_error) = persist_input_playlist_with_options(
                &ctx.config,
                input,
                merged_groups,
                InputPlaylistPersistOptions { accepted_empty_clusters },
            )
            .await;
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

    if mark_as_processed
        && !playlist_download_result.partial
        && error.is_none()
        && (!playlist.is_empty() || !accepted_empty_clusters.is_empty() || authoritative_library)
    {
        // Mark after persist/load so other workers only see this input as ready when data is usable.
        ctx.mark_input_downloaded(input.name.clone()).await;
    }

    if !playlist_download_result.persisted && error.is_none() {
        report_force_updates(
            &ctx.events,
            &ctx.run_id,
            ctx.execution_order,
            input,
            &playlist_download_result.force_updates,
        );
    }

    finalize_input_telemetry(&mut playlist_download_result, error.as_ref());
    let request_policy = ctx
        .input_refresh
        .filter(|input_refresh| input_refresh.input_id == input.id)
        .map(|input_refresh| input_refresh.policy);
    persist_finalized_cluster_snapshots(&mut playlist_download_result, request_policy, error.as_ref());

    // Explicitly release per-input lock after load/persist/mark steps are completed.
    drop(input_lock);

    InputDownloadResult {
        authoritative_library,
        errors: playlist_download_result.download_err,
        source: playlist,
        storage_error: error,
        partial: playlist_download_result.partial,
        quality_rejections: playlist_download_result.quality_rejections,
        accepted_empty_clusters,
        input_telemetry: playlist_download_result.input_telemetry,
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
