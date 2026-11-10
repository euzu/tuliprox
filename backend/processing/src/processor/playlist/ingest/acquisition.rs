use super::{
    super::transform::group_mapped_items,
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
    CacheStatusScope, InputDownloadResult, InputRunOutcome, LibraryProvider, MediaServerProvider,
    PendingInputStatusUpdate, PlaylistDownloadResult, PlaylistProcessingContext, PlaylistUpdateExecutionRef,
    StalkerProvider, PIPELINE_TRANSPARENCY_CLUSTERS,
};
use crate::{input_cache, metadata_sink::MetadataUpdateSink, processor::StalkerRefreshMode};
use log::{debug, warn};
use shared::{
    error::TuliproxError,
    model::{
        ClusterFlags, EventMessage, EventSink, HookCtx, InputRefreshPolicy, InputType, PlaylistItem,
        PlaylistUpdateDataSource, PlaylistUpdateProgressEvent, PluginAbort, PluginHook, PluginHost, PluginInputRequest,
        PluginRunReport, ProviderFetchFailure, VirtualId,
    },
    utils::sanitize_sensitive_info,
};
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, ConfigInput};
use tuliprox_iptv::{
    error::ProviderErrorKind,
    provider::{
        BatchContainerProvider, M3uProvider, PlaylistFetch, PlaylistFetchRequest, PlaylistProvider,
        UnsupportedProvider, XtreamProvider,
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

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn playlist_download_from_input<E: EventSink, P: PluginHost>(
    client: &reqwest::Client,
    app_config: &Arc<AppConfig>,
    events: &E,
    plugins: &P,
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
    let status = input_cache::load_input_status(&storage_path);
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

    let mut pending_status = PendingInputStatusUpdate::new(storage_path, status, cluster_status_source);
    if fully_cached {
        return cached_playlist_download_result(input, refresh_policy).with_pending_status(pending_status);
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
        InputType::Plex | InputType::Emby | InputType::Jellyfin => MediaServerProvider.fetch(&request).await,
        InputType::Plugin => plugin_playlist_fetch(plugins, input).await,
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
    apply_playlist_fetch_outcome(
        events,
        execution.run_id,
        execution.execution_order,
        input,
        &mut pending_status.status,
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
    PlaylistDownloadResult::from(fetch).with_input_telemetry(input_telemetry).with_pending_status(pending_status)
}

/// `type: plugin`: the playlist is whatever the named plugin's `fetch-playlist` returns, grouped the
/// way a parsed M3U is. The plugin gets the input's `url` and credentials as configured (all optional
/// for a plugin input). A plugin `Err`, trap or timeout fails the input like an unreachable provider;
/// the downstream path (persist, cache status, `on-playlist-loaded`) is the M3U one.
async fn plugin_playlist_fetch<P: PluginHost>(plugins: &P, input: &ConfigInput) -> PlaylistFetch {
    let Some(plugin) = input.plugin.as_deref() else {
        return PlaylistFetch::failed(TuliproxError::ConfigInput(format!(
            "plugin input '{}' does not name a plugin",
            input.name
        )));
    };
    // A plugin that was uninstalled or disabled leaves the input inactive: say so once, in the words of
    // the operator's own fix, instead of failing on the provider's generic answer. The input keeps its
    // last playlist (an empty result is never persisted) and every other input and target runs.
    if !plugins.provides(PluginHook::FetchInput, plugin) {
        warn!(
            "plugin input '{}' is inactive: plugin '{plugin}' is not installed or not enabled. Install it under \
             Plugins, or remove the input.",
            input.name
        );
        return PlaylistFetch::failed(TuliproxError::ConfigInput(format!(
            "plugin input '{}' is inactive: plugin '{plugin}' is not installed or not enabled",
            input.name
        )));
    }
    let ctx = HookCtx::new("").with_input(&input.name);
    let request = PluginInputRequest {
        input_name: &input.name,
        url: &input.url,
        username: input.username.as_deref(),
        password: input.password.as_deref(),
        options: &input.plugin_options,
    };
    match plugins.fetch_playlist(plugin, &ctx, &request).await {
        Ok(items) => PlaylistFetch::groups(group_mapped_items(items)),
        Err(err) => PlaylistFetch::failed(TuliproxError::Download(format!(
            "plugin input '{}': {}",
            input.name,
            sanitize_sensitive_info(&err)
        ))),
    }
}

/// This read-modify-write operation requires holding the per-input lock from
/// `PlaylistProcessingContext::get_input_lock` (as done in `download_input`).
pub(crate) async fn invalidate_input_cache_status<
    E: EventSink + Clone + 'static,
    M: MetadataUpdateSink,
    P: PluginHost,
>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &ConfigInput,
) {
    let storage_dir = { ctx.config.config.load().storage_dir.clone() };
    let storage_path = input_cache::resolve_input_storage_path(&storage_dir, &input.name).await;
    input_cache::update_input_status(&storage_path, |status| {
        let changed = !status.clusters.is_empty();
        status.clusters.clear();
        changed
    })
    .await;
}

pub(crate) async fn load_cached_input_playlist<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &Arc<ConfigInput>,
) -> (PlaylistSource, Option<TuliproxError>) {
    match load_input_playlist(&ctx.config, input, None).await {
        Ok(pl_source) => (pl_source, None),
        Err(err) => (MemoryPlaylistSource::default().into_source(), Some(err)),
    }
}

/// The result of an input that another source of this run already fetched without success.
fn failed_earlier_in_run(input: &ConfigInput, failure: Option<TuliproxError>) -> InputDownloadResult {
    debug!("Input '{}' failed earlier in this run; not fetched again", input.name);
    InputDownloadResult::empty(failure.into_iter().collect())
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn download_input<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &Arc<ConfigInput>,
    allow_staged_input: bool,
) -> InputDownloadResult {
    if input.staged.is_some() && !allow_staged_input {
        return InputDownloadResult::empty(Vec::new());
    }

    let staged_overlay = if input.staged.is_none() {
        let sources = ctx.config.sources.load();
        sources.get_staged_input_for_provider(&input.name).cloned()
    } else {
        None
    };

    // Coordination Logic. An input that already failed in this run returns its failure at once, without
    // waiting for the input lock: asking the provider again would only repeat the request, and could give each
    // source a different playlist.
    let outcome = ctx.input_outcome(&input.name).await;
    if let Some(InputRunOutcome::Failed(failure)) = outcome {
        return failed_earlier_in_run(input, failure);
    }
    let need_download = outcome.is_none();
    // Keep this lock for the whole critical section (download + persist/load + mark processed)
    // so parallel sources sharing the same input cannot observe a half-written state.
    let mut input_lock = if need_download { Some(ctx.get_input_lock(&input.name).await) } else { None };
    let mut mark_as_processed = false;
    let refresh_policy = ctx.refresh_policy(input.id);

    let mut playlist_download_result = if need_download {
        // Check again after lock: a parallel source may have finished the input meanwhile.
        let locked_outcome = ctx.input_outcome(&input.name).await;
        if let Some(InputRunOutcome::Failed(failure)) = locked_outcome {
            return failed_earlier_in_run(input, failure);
        }

        if locked_outcome.is_some() {
            // Use empty results, will load from disk below
            in_run_reuse_playlist_download_result()
        } else if ctx.is_input_in_maintenance(&input.name) {
            // Keep serving what was fetched last; the provider is out of service. This also
            // covers the Stalker refresh, which only runs inside the provider download.
            debug!("Input '{}' is in maintenance; using its persisted playlist", input.name);
            mark_as_processed = true;
            cached_playlist_download_result(input, refresh_policy)
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
                &ctx.plugins,
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
        if must_force_refresh && ctx.is_input_in_maintenance(&input.name) {
            // A refresh would contact the provider; without a persisted playlist the input
            // contributes nothing until maintenance ends.
            warn!("Input '{}' is in maintenance and has no readable persisted playlist; skipping it", input.name);
            preloaded_playlist = Some((MemoryPlaylistSource::default().into_source(), None));
        } else if must_force_refresh {
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
                        &ctx.plugins,
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

    // `on-playlist-loaded`, once per input, after the staged overlay so a plugin
    // sees what the targets would otherwise consume. A staged input is hooked as
    // part of the provider input it overlays, never on its own.
    let mut plugins = PluginRunReport::default();
    if error.is_none() && input.staged.is_none() {
        match apply_playlist_loaded_hook(&ctx.plugins, &input.name, playlist, &mut plugins).await {
            Ok(hooked) => playlist = hooked,
            Err((original, abort)) => {
                playlist = original;
                error = Some(TuliproxError::Plugin(abort.to_string()));
            }
        }
    }

    // Managed contributions are processing overlays, not provider-cache writes. Run on
    // cached inputs too, after inventory hooks, so providers never ingest their own output.
    if error.is_none() && input.staged.is_none() {
        match apply_managed_channels(&ctx.plugins, &input.name, &input.managed_channels, playlist, &mut plugins).await {
            Ok(overlaid) => playlist = overlaid,
            Err((original, abort)) => {
                playlist = original;
                error = Some(TuliproxError::Plugin(abort.to_string()));
            }
        }
        playlist = filter_skipped_clusters_from_source(playlist, input);
    }

    if input.input_type == InputType::M3u {
        let alias_errors = download_m3u_alias_playlists(ctx, input).await;
        playlist_download_result.download_err.extend(alias_errors);
    }

    let replacement_failure = if ctx.replace_target_contents {
        if !playlist_download_result.quality_rejections.is_empty() {
            Some(TuliproxError::RepositoryPlaylist(format!(
                "Input '{}' was rejected by the quality policy",
                input.name
            )))
        } else if ctx.is_input_in_maintenance(&input.name) {
            Some(TuliproxError::RepositoryPlaylist(format!("Input '{}' is in maintenance", input.name)))
        } else {
            playlist_download_result.download_err.first().cloned().or_else(|| error.clone())
        }
    } else {
        None
    };
    if let Some(failure) = replacement_failure.as_ref() {
        // Reuse in a second source must not turn a fallback into a successful build.
        ctx.mark_input_failed(input.name.clone(), Some(failure.clone())).await;
    } else if mark_as_processed
        && !playlist_download_result.partial
        && error.is_none()
        && (!playlist.is_empty() || !accepted_empty_clusters.is_empty() || authoritative_library)
    {
        // Mark after persist/load so other workers only see this input as ready when data is usable.
        ctx.mark_input_downloaded(input.name.clone()).await;
    } else if mark_as_processed && !playlist_download_result.partial {
        // A partial (resumable) input may continue in a later source; a failed one is not fetched again.
        let failure = playlist_download_result.download_err.first().or(error.as_ref()).cloned();
        ctx.mark_input_failed(input.name.clone(), failure).await;
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
    persist_finalized_cluster_snapshots(&mut playlist_download_result, request_policy, error.as_ref()).await;

    // Explicitly release per-input lock after load/persist/mark steps are completed.
    drop(input_lock);

    if ctx.replace_target_contents
        && ctx.is_input_in_maintenance(&input.name)
        && playlist_download_result.download_err.is_empty()
    {
        playlist_download_result.download_err.extend(replacement_failure);
    }

    InputDownloadResult {
        authoritative_library,
        errors: playlist_download_result.download_err,
        source: playlist,
        storage_error: error,
        partial: playlist_download_result.partial,
        quality_rejections: playlist_download_result.quality_rejections,
        accepted_empty_clusters,
        input_telemetry: playlist_download_result.input_telemetry,
        plugins,
    }
}

/// Append a complete managed view, preserving disk-backed providers as lazy sources.
pub(crate) async fn apply_managed_channels<P: PluginHost>(
    plugins: &P,
    input_name: &str,
    bindings: &[shared::model::PluginRef],
    playlist: PlaylistSource,
    report: &mut PluginRunReport,
) -> Result<PlaylistSource, (PlaylistSource, PluginAbort)> {
    let inputs = vec![input_name.to_string()];
    let request = shared::model::ManagedChannelRequest {
        destination: shared::model::ManagedDestination::Input,
        name: input_name,
        stage: None,
        inputs: &inputs,
    };
    let mut items = Vec::new();
    for binding in bindings {
        match plugins.managed_channels(binding.id(), &request, report).await {
            Ok(mut contribution) => items.append(&mut contribution),
            Err(abort) => return Err((playlist, abort)),
        }
    }
    for item in &mut items {
        item.header.source_ordinal = u32::MAX;
    }
    Ok(playlist.append_groups(group_mapped_items(items)))
}

/// Run `on-playlist-loaded` over the items of `playlist` that a plugin's scope
/// matches (plan §7 *Sync/async boundary*).
///
/// Disk-backed sources are only scanned (`items()`) to find out whether
/// anything matches; when nothing does, the source is returned untouched.
/// Otherwise the groups are materialized, the matching items alone go to the
/// hook, and the result is written back into an in-memory source: dropped
/// items disappear, edited items stay in their original position and are
/// regrouped by their (possibly rewritten) group. Survivors keep their
/// relative order and are re-associated with their original slot by a per-call
/// handle assigned before the call (plugins cannot change host-owned fields).
///
/// A [`PluginAbort`] hands the unchanged source back with the abort, so the
/// input fails through the normal error path with its data intact.
///
/// A disk-backed source cannot be written back item by item, so the hook has to
/// hold the whole playlist in memory, and the result stays in memory. Above
/// [`DISK_PLAYLIST_HOOK_CEILING`] items the hook is skipped with one warning
/// and the playlist stays on disk: a plugin never costs more memory than that.
pub(crate) async fn apply_playlist_loaded_hook<P: PluginHost>(
    plugins: &P,
    input_name: &str,
    playlist: PlaylistSource,
    report: &mut PluginRunReport,
) -> Result<PlaylistSource, (PlaylistSource, PluginAbort)> {
    apply_playlist_loaded_hook_within(plugins, input_name, playlist, report, DISK_PLAYLIST_HOOK_CEILING).await
}

/// Items a disk-backed playlist may have for `on-playlist-loaded` to run over it.
/// About 100 MiB of interned items at most, which the smallest supported hosts
/// (armv7) can still take next to a processing run.
const DISK_PLAYLIST_HOOK_CEILING: usize = 250_000;

async fn apply_playlist_loaded_hook_within<P: PluginHost>(
    plugins: &P,
    input_name: &str,
    mut playlist: PlaylistSource,
    report: &mut PluginRunReport,
    disk_ceiling: usize,
) -> Result<PlaylistSource, (PlaylistSource, PluginAbort)> {
    let ctx = HookCtx::new("").with_input(input_name);
    let Some(scopes) = plugins.scope(PluginHook::PlaylistLoaded, &ctx) else {
        return Ok(playlist);
    };
    // Checked before the scope scan, which would read a huge disk playlist in full.
    if !playlist.is_memory() {
        let count = playlist.get_channel_count();
        if count > disk_ceiling {
            warn!(
                "Input '{input_name}': on-playlist-loaded skipped, the disk-backed playlist has {count} items (limit \
                 {disk_ceiling})"
            );
            return Ok(playlist);
        }
    }
    if !scopes.is_all() && !playlist.items().any(|item| scopes.matches(&item)) {
        return Ok(playlist);
    }

    let groups = playlist.take_groups();
    let mut slots: Vec<Option<PlaylistItem>> = Vec::new();
    let mut matched: Vec<usize> = Vec::new();
    let mut selected: Vec<PlaylistItem> = Vec::new();
    let mut original_virtual_ids: Vec<VirtualId> = Vec::new();
    for mut item in groups.into_iter().flat_map(|group| group.channels) {
        if scopes.matches(&item) {
            matched.push(slots.len());
            original_virtual_ids.push(item.header.virtual_id);
            // Safe to borrow virtual_id as the per-call handle: virtual ids are not
            // assigned yet at ingest, guests never see or change virtual_id (WIT carries
            // its own `handle`), and the original value is restored on every path.
            let handle = u32::try_from(selected.len()).unwrap_or(u32::MAX);
            item.header.virtual_id = VirtualId::new(handle);
            selected.push(item);
            slots.push(None);
        } else {
            slots.push(Some(item));
        }
    }

    let outcome = plugins.on_playlist_loaded(&ctx, &mut selected, report).await;
    if let Err(abort) = outcome {
        // The input fails; hand back whatever is left so nothing is lost.
        for mut item in selected {
            let orig_idx = usize::try_from(item.header.virtual_id.get()).unwrap_or(usize::MAX);
            if orig_idx < matched.len() {
                item.header.virtual_id = original_virtual_ids[orig_idx];
                slots[matched[orig_idx]] = Some(item);
            }
        }
        let rest = group_mapped_items(slots.into_iter().flatten().collect());
        return Err((MemoryPlaylistSource::new(rest).into_source(), abort));
    }

    for mut survivor in selected {
        let orig_idx = usize::try_from(survivor.header.virtual_id.get()).unwrap_or(usize::MAX);
        if orig_idx < matched.len() {
            survivor.header.virtual_id = original_virtual_ids[orig_idx];
            slots[matched[orig_idx]] = Some(survivor);
        }
    }
    let items: Vec<PlaylistItem> = slots.into_iter().flatten().collect();
    Ok(MemoryPlaylistSource::new(group_mapped_items(items)).into_source())
}

async fn download_m3u_alias_playlists<E: EventSink + Clone + 'static, M: MetadataUpdateSink, P: PluginHost>(
    ctx: &PlaylistProcessingContext<E, M, P>,
    input: &ConfigInput,
) -> Vec<TuliproxError> {
    let Some(aliases) = input.get_enabled_aliases() else { return vec![] };
    // Maintenance is keyed by the parent input; an alias carries its own name and would
    // otherwise contact the provider. Its persisted playlist stays in place.
    if ctx.is_input_in_maintenance(&input.name) {
        debug!("Input '{}' is in maintenance; skipping its M3U alias downloads", input.name);
        return vec![];
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::NoPlugins;
    use std::sync::Arc;

    #[derive(Clone)]
    struct ManagedOverlay;
    impl PluginHost for ManagedOverlay {
        #[allow(clippy::unused_async_trait_impl, reason = "a scripted host answers without awaiting")]
        async fn managed_channels(
            &self,
            _plugin: &str,
            request: &shared::model::ManagedChannelRequest<'_>,
            _report: &mut PluginRunReport,
        ) -> Result<Vec<PlaylistItem>, PluginAbort> {
            if request.name != "provider" {
                return Ok(vec![]);
            }
            Ok(vec![PlaylistItem {
                header: shared::model::PlaylistItemHeader {
                    id: "plugin:managed".into(),
                    input_stream_id: "plugin:managed".into(),
                    input_name: "provider".into(),
                    name: "Team channel".into(),
                    title: "Team channel".into(),
                    group: "Sports".into(),
                    url: "https://stream.test/team.ts".into(),
                    ..Default::default()
                },
            }])
        }
    }

    #[tokio::test]
    async fn managed_overlay_refreshes_a_cached_input_without_accumulating_or_modifying_provider_items() {
        let provider = PlaylistItem {
            header: shared::model::PlaylistItemHeader {
                id: "1".into(),
                input_stream_id: "1".into(),
                input_name: "provider".into(),
                name: "Provider channel".into(),
                title: "Provider channel".into(),
                group: "News".into(),
                url: "https://stream.test/provider.ts".into(),
                ..Default::default()
            },
        };
        let mut cached = MemoryPlaylistSource::new(group_mapped_items(vec![provider])).into_source();
        for _ in 0..2 {
            let source = cached.clone_source().unwrap();
            let mut result = apply_managed_channels(
                &ManagedOverlay,
                "provider",
                &[shared::model::PluginRef::new("bridge").unwrap()],
                source,
                &mut PluginRunReport::default(),
            )
            .await
            .ok()
            .unwrap();
            assert_eq!(result.get_channel_count(), 2);
            assert_eq!(result.get_group_count(), 2);
            let originals: Vec<_> =
                result.items().filter(|item| &*item.header.id == "1").map(std::borrow::Cow::into_owned).collect();
            assert_eq!(originals.len(), 1);
            assert_eq!(&*originals[0].header.name, "Provider channel");
            assert_eq!(&*originals[0].header.url, "https://stream.test/provider.ts");
        }
        assert_eq!(cached.get_channel_count(), 1, "cached provider source must remain unchanged");
        let mut other = apply_managed_channels(
            &ManagedOverlay,
            "other",
            &[shared::model::PluginRef::new("bridge").unwrap()],
            cached.clone_source().unwrap(),
            &mut PluginRunReport::default(),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(other.get_channel_count(), 1);
        let mut empty = apply_managed_channels(
            &ManagedOverlay,
            "provider",
            &[shared::model::PluginRef::new("bridge").unwrap()],
            PlaylistSource::default(),
            &mut PluginRunReport::default(),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(empty.get_channel_count(), 1, "empty provider playlists still accept managed channels");
    }

    fn plugin_input(plugin: Option<&str>) -> ConfigInput {
        ConfigInput {
            name: Arc::from("youtbepl"),
            input_type: InputType::Plugin,
            plugin: plugin.map(Arc::from),
            ..ConfigInput::default()
        }
    }

    /// A plugin input whose plugin was uninstalled or disabled fails only itself, with a message that
    /// says what to do. It never reaches the fetch, so it cannot disturb the other inputs.
    #[tokio::test]
    async fn a_plugin_input_without_its_plugin_reports_itself_inactive() {
        let fetch = plugin_playlist_fetch(&NoPlugins, &plugin_input(Some("youtube-input"))).await;

        assert!(fetch.groups.is_empty());
        let message = fetch.errors.first().map(ToString::to_string).unwrap_or_default();
        assert!(message.contains("youtbepl"), "the input is named: {message}");
        assert!(message.contains("youtube-input"), "the plugin is named: {message}");
        assert!(message.contains("is not installed or not enabled"), "{message}");
    }

    #[tokio::test]
    async fn a_plugin_input_must_name_a_plugin() {
        let fetch = plugin_playlist_fetch(&NoPlugins, &plugin_input(None)).await;
        let message = fetch.errors.first().map(ToString::to_string).unwrap_or_default();
        assert!(message.contains("does not name a plugin"), "{message}");
    }
}
