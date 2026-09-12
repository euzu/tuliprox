#![allow(clippy::wildcard_imports)]
use super::{
    fetch_outcome::{apply_playlist_fetch_outcome, report_force_updates, CacheStatusScope},
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
    pub force_updates: Vec<ClusterForceUpdate>,
    pub was_cached: bool,
    pub persisted: bool,
    pub partial: bool,
    pub input_telemetry: Option<PlaylistUpdateInputTelemetry>,
    pending_status: Option<PendingInputStatusUpdate>,
    provider_failure_evidence: ProviderFailureEvidence,
}

struct PendingInputStatusUpdate {
    storage_path: PathBuf,
    status: input_cache::InputStatus,
    status_changed: bool,
    cluster_status_source: ClusterStatusSource,
}

#[derive(Clone, Copy)]
enum ClusterStatusSource {
    PerCluster,
    Default,
}

#[derive(Clone, Copy, Default)]
enum ProviderFailureEvidence {
    #[default]
    None,
    TechnicalFailure,
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
            force_updates: Vec::new(),
            was_cached,
            persisted,
            partial: false,
            input_telemetry: None,
            pending_status: None,
            provider_failure_evidence: ProviderFailureEvidence::None,
        }
    }

    fn with_input_telemetry(mut self, input_telemetry: PlaylistUpdateInputTelemetry) -> Self {
        self.input_telemetry = Some(input_telemetry);
        self
    }

    fn with_pending_status(mut self, pending_status: PendingInputStatusUpdate) -> Self {
        self.pending_status = Some(pending_status);
        self
    }
}

impl From<PlaylistFetch> for PlaylistDownloadResult {
    fn from(fetch: PlaylistFetch) -> Self {
        let provider_failure_evidence = if fetch.partial || !fetch.errors.is_empty() {
            ProviderFailureEvidence::TechnicalFailure
        } else {
            ProviderFailureEvidence::None
        };
        Self {
            downloaded_playlist: fetch.groups,
            download_err: fetch.errors,
            quality_rejections: fetch.quality_rejections,
            force_updates: fetch.force_updates,
            was_cached: false,
            persisted: fetch.persisted,
            partial: fetch.partial,
            input_telemetry: None,
            pending_status: None,
            provider_failure_evidence,
        }
    }
}

pub(super) const PIPELINE_TRANSPARENCY_CLUSTERS: [XtreamCluster; 3] =
    [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series];

fn is_cluster_input(input_type: InputType) -> bool {
    matches!(input_type, InputType::Xtream | InputType::Stalker | InputType::M3u)
}

pub(super) fn cluster_is_configured(input: &ConfigInput, cluster: XtreamCluster) -> bool {
    let skip_flag = match cluster {
        XtreamCluster::Live => ConfigInputFlags::SkipLive,
        XtreamCluster::Video => ConfigInputFlags::SkipVod,
        XtreamCluster::Series => ConfigInputFlags::SkipSeries,
    };
    !input.has_flag(skip_flag)
}

fn input_telemetry_for_fetch(
    input: &ConfigInput,
    refresh_policy: InputRefreshPolicy,
    source: PlaylistUpdateDataSource,
    provider_clusters: &[XtreamCluster],
    fetch: Option<&PlaylistFetch>,
) -> PlaylistUpdateInputTelemetry {
    let input_type = input.get_download_input_type();
    if !is_cluster_input(input_type) {
        return PlaylistUpdateInputTelemetry { refresh_policy, source: Some(source), clusters: Vec::new() };
    }

    let mut candidate_counts = HashMap::<XtreamCluster, usize>::new();
    if let Some(fetch) = fetch {
        for group in &fetch.groups {
            *candidate_counts.entry(group.xtream_cluster).or_default() += group.channels.len();
        }
        if fetch.errors.is_empty() && !fetch.partial && !fetch.persisted {
            for cluster in provider_clusters {
                candidate_counts.entry(*cluster).or_default();
            }
        }
    }

    let clusters = PIPELINE_TRANSPARENCY_CLUSTERS
        .into_iter()
        .map(|cluster| {
            let requested = cluster_is_configured(input, cluster);
            let source = requested.then(|| {
                if provider_clusters.contains(&cluster) {
                    PlaylistUpdateDataSource::Provider
                } else {
                    PlaylistUpdateDataSource::Cache
                }
            });
            let configured_threshold =
                input.options.as_ref().map_or(0, |options| options.update_quality.threshold(cluster));
            let mut telemetry = PlaylistUpdateClusterTelemetry {
                cluster,
                requested,
                source,
                baseline_count: None,
                candidate_count: candidate_counts.get(&cluster).copied(),
                active_count: None,
                threshold: (configured_threshold > 0).then_some(configured_threshold),
                quality: None,
                decision: None,
                technical_state: None,
            };

            if let Some(fetch) = fetch {
                if requested && fetch.failed_clusters.contains(&cluster) {
                    telemetry.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Failed);
                }
                if let Some(rejection) = fetch.quality_rejections.iter().find(|item| item.cluster == cluster) {
                    telemetry.baseline_count = Some(rejection.current_count);
                    telemetry.candidate_count = Some(rejection.candidate_count);
                    telemetry.active_count = Some(rejection.current_count);
                    telemetry.threshold = Some(rejection.threshold);
                    telemetry.quality = Some(rejection.quality);
                    telemetry.decision = Some(PlaylistUpdateClusterDecision::Rejected);
                } else if let Some(acceptance) = fetch.quality_acceptances.iter().find(|item| item.cluster == cluster) {
                    telemetry.baseline_count = acceptance.current_count;
                    telemetry.candidate_count = Some(acceptance.candidate_count);
                    telemetry.threshold = Some(acceptance.threshold);
                    telemetry.quality = acceptance.quality;
                    telemetry.decision = Some(PlaylistUpdateClusterDecision::Accepted);
                } else if let Some(force_update) = fetch.force_updates.iter().find(|item| item.cluster == cluster) {
                    telemetry.candidate_count = Some(force_update.candidate_count);
                    telemetry.threshold =
                        (force_update.configured_threshold > 0).then_some(force_update.configured_threshold);
                    telemetry.decision = Some(PlaylistUpdateClusterDecision::Accepted);
                } else if source == Some(PlaylistUpdateDataSource::Provider) {
                    if configured_threshold == 0
                        && (candidate_counts.contains_key(&cluster) || (fetch.errors.is_empty() && !fetch.partial))
                    {
                        telemetry.decision = Some(PlaylistUpdateClusterDecision::Accepted);
                    } else if (provider_clusters == [cluster] || input_type == InputType::M3u)
                        && fetch.groups.is_empty()
                        && !fetch.persisted
                        && !fetch.errors.is_empty()
                    {
                        telemetry.decision = Some(PlaylistUpdateClusterDecision::TechnicalError);
                    }
                }
            }
            telemetry
        })
        .collect();

    PlaylistUpdateInputTelemetry { refresh_policy, source: None, clusters }
}

fn confirm_published_cluster_counts(input_telemetry: &mut PlaylistUpdateInputTelemetry) {
    for cluster in &mut input_telemetry.clusters {
        if cluster.decision == Some(PlaylistUpdateClusterDecision::Accepted) {
            cluster.active_count = cluster.candidate_count;
        }
    }
}

fn finalize_input_telemetry(
    playlist_download_result: &mut PlaylistDownloadResult,
    storage_error: Option<&TuliproxError>,
) {
    let activation_is_uncertain = storage_error.is_some()
        || playlist_download_result.partial
        || !playlist_download_result.download_err.is_empty();
    if let Some(input_telemetry) = playlist_download_result.input_telemetry.as_mut() {
        if activation_is_uncertain {
            for cluster in &mut input_telemetry.clusters {
                cluster.active_count = None;
            }
        } else {
            confirm_published_cluster_counts(input_telemetry);
        }
    }
}

pub(crate) fn neutralize_overlaid_cluster_facts(
    input_telemetry: &mut PlaylistUpdateInputTelemetry,
    overlaid_clusters: ClusterFlags,
) {
    for cluster in &mut input_telemetry.clusters {
        if cluster_selected(cluster.cluster, overlaid_clusters) {
            cluster.source = None;
            cluster.baseline_count = None;
            cluster.candidate_count = None;
            cluster.active_count = None;
            cluster.quality = None;
            cluster.decision = None;
            cluster.technical_state = None;
        }
    }
}

fn persisted_quality_snapshot(
    cluster: &PlaylistUpdateClusterTelemetry,
    effective_policy: InputRefreshPolicy,
) -> Option<PersistedPlaylistUpdateQualitySnapshot> {
    if effective_policy.bypasses_quality() {
        return None;
    }
    let threshold = cluster.threshold?;
    let decision = match cluster.decision? {
        PlaylistUpdateClusterDecision::Accepted => PersistedPlaylistUpdateQualityDecision::Accepted,
        PlaylistUpdateClusterDecision::Rejected => PersistedPlaylistUpdateQualityDecision::Rejected,
        PlaylistUpdateClusterDecision::TechnicalError => return None,
    };
    Some(PersistedPlaylistUpdateQualitySnapshot {
        threshold,
        baseline_count: cluster.baseline_count,
        candidate_count: cluster.candidate_count,
        achieved_quality: cluster.quality,
        decision,
    })
}

#[derive(Clone, Copy)]
struct PersistedSnapshotContext {
    effective_policy: InputRefreshPolicy,
    request_policy: Option<InputRefreshPolicy>,
    technical_failure: bool,
    storage_failure: bool,
    provider_failure_evidence: ProviderFailureEvidence,
    provider_cluster_count: usize,
}

fn persisted_technical_state(
    cluster: &PlaylistUpdateClusterTelemetry,
    context: PersistedSnapshotContext,
) -> Option<PersistedPlaylistUpdateTechnicalState> {
    let source = cluster.source?;
    if cluster.technical_state == Some(PersistedPlaylistUpdateTechnicalState::Failed) {
        return cluster.technical_state;
    }
    if cluster.decision == Some(PlaylistUpdateClusterDecision::TechnicalError) {
        return Some(PersistedPlaylistUpdateTechnicalState::Failed);
    }
    if context.storage_failure && source == PlaylistUpdateDataSource::Provider {
        return Some(PersistedPlaylistUpdateTechnicalState::Failed);
    }
    if !context.technical_failure {
        return Some(PersistedPlaylistUpdateTechnicalState::Succeeded);
    }
    (source == PlaylistUpdateDataSource::Provider
        && context.provider_cluster_count == 1
        && matches!(context.provider_failure_evidence, ProviderFailureEvidence::TechnicalFailure))
    .then_some(PersistedPlaylistUpdateTechnicalState::Failed)
}

fn persisted_cluster_snapshot(
    cluster: &PlaylistUpdateClusterTelemetry,
    context: PersistedSnapshotContext,
) -> PersistedPlaylistUpdateClusterSnapshot {
    let quality = persisted_quality_snapshot(cluster, context.effective_policy);
    PersistedPlaylistUpdateClusterSnapshot {
        policy: cluster.source.and(context.request_policy),
        source: cluster.source,
        quality_guard_threshold: (quality.is_none() && cluster.source.is_some())
            .then_some(cluster.threshold.unwrap_or(0)),
        quality,
        active_count: cluster.active_count,
        technical_state: persisted_technical_state(cluster, context),
    }
}

fn persist_finalized_cluster_snapshots(
    playlist_download_result: &mut PlaylistDownloadResult,
    request_policy: Option<InputRefreshPolicy>,
    storage_error: Option<&TuliproxError>,
) {
    let technical_failure = storage_error.is_some()
        || playlist_download_result.partial
        || !playlist_download_result.download_err.is_empty();
    let provider_failure_evidence = playlist_download_result.provider_failure_evidence;
    let snapshots = playlist_download_result.input_telemetry.as_ref().map_or_else(Vec::new, |telemetry| {
        let provider_cluster_count = telemetry
            .clusters
            .iter()
            .filter(|cluster| cluster.requested && cluster.source == Some(PlaylistUpdateDataSource::Provider))
            .count();
        telemetry
            .clusters
            .iter()
            .filter(|cluster| cluster.requested)
            .map(|cluster| {
                let snapshot = persisted_cluster_snapshot(
                    cluster,
                    PersistedSnapshotContext {
                        effective_policy: telemetry.refresh_policy,
                        request_policy,
                        technical_failure,
                        storage_failure: storage_error.is_some(),
                        provider_failure_evidence,
                        provider_cluster_count,
                    },
                );
                (cluster.cluster, snapshot)
            })
            .collect()
    });

    let Some(mut pending_status) = playlist_download_result.pending_status.take() else {
        return;
    };
    let mut changed = pending_status.status_changed;
    for (cluster, snapshot) in snapshots {
        changed |= match pending_status.cluster_status_source {
            ClusterStatusSource::PerCluster => {
                input_cache::replace_cluster_snapshot(&mut pending_status.status, cluster.as_ref(), snapshot)
            }
            ClusterStatusSource::Default => input_cache::replace_cluster_snapshot_from_default_status(
                &mut pending_status.status,
                cluster.as_ref(),
                snapshot,
            ),
        };
    }
    if changed {
        input_cache::save_input_status(&pending_status.storage_path, &pending_status.status);
    }
}

fn cached_playlist_download_result(input: &ConfigInput, refresh_policy: InputRefreshPolicy) -> PlaylistDownloadResult {
    PlaylistDownloadResult::new(vec![], vec![], true, false).with_input_telemetry(input_telemetry_for_fetch(
        input,
        refresh_policy,
        PlaylistUpdateDataSource::Cache,
        &[],
        None,
    ))
}

/// Loads an input already acquired in this run without reporting a second acquisition.
fn in_run_reuse_playlist_download_result() -> PlaylistDownloadResult {
    PlaylistDownloadResult::new(vec![], vec![], true, false)
}

fn forced_empty_cluster_flags(force_updates: &[ClusterForceUpdate]) -> ClusterFlags {
    force_updates.iter().filter(|update| update.candidate_count == 0).fold(
        ClusterFlags::empty(),
        |mut clusters, update| {
            clusters.insert(match update.cluster {
                XtreamCluster::Live => ClusterFlags::Live,
                XtreamCluster::Video => ClusterFlags::Vod,
                XtreamCluster::Series => ClusterFlags::Series,
            });
            clusters
        },
    )
}

pub(crate) fn collect_effective_skip_clusters(input: &ConfigInput) -> Vec<XtreamCluster> {
    if !input.input_type.is_xtream() && input.input_type != InputType::M3u {
        return vec![];
    }
    xtream::get_skip_cluster(input)
}

fn report_forced_update_request<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    input: &ConfigInput,
    refresh_policy: InputRefreshPolicy,
) {
    if !refresh_policy.bypasses_quality() {
        return;
    }
    let input_type = input.get_download_input_type();
    if !is_cluster_input(input_type) {
        return;
    }
    let clusters = [
        (ConfigInputFlags::SkipLive, "live"),
        (ConfigInputFlags::SkipVod, "vod"),
        (ConfigInputFlags::SkipSeries, "series"),
    ]
    .into_iter()
    .filter_map(|(skip_flag, name)| (!input.has_flag(skip_flag)).then_some(name))
    .collect::<Vec<_>>()
    .join(",");
    let message =
        format!("Input '{}': forced update requested; cache and update-quality bypassed for {clusters}", input.name);
    info!("{message}");
    ctx.events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::for_run_input(
        ctx.run_id.clone(),
        ctx.execution_order,
        input.id,
        input.name.to_string(),
        message,
    )));
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

#[derive(Clone, Copy)]
struct PlaylistUpdateExecutionRef<'a> {
    run_id: &'a PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
}

/// Local discovery is not a provider Refresh policy. Both can require a fresh read.
#[derive(Clone, Copy)]
enum InputAcquisition {
    Provider(InputRefreshPolicy),
    RescannedLibrary,
}

impl InputAcquisition {
    fn refresh_policy(self) -> InputRefreshPolicy {
        match self {
            Self::Provider(policy) => policy,
            Self::RescannedLibrary => InputRefreshPolicy::NORMAL,
        }
    }

    fn bypasses_cache(self) -> bool {
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
            super::m3u_quality::effective_m3u_fetch(
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
    pub(super) fn update_state(&mut self) -> PlaylistUpdateState {
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

// The same completion contract applies to a source job (including EPG errors)
// and an indirect staged download, which has no separate source job.
struct InputCompletionFacts {
    job_state: InputJobState,
    had_errors: bool,
    had_quality_rejections: bool,
}

impl InputCompletionFacts {
    fn update_state(self) -> PlaylistUpdateState {
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

pub(super) fn report_input_completion<E: EventSink>(
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
            super::input_status::persist_input_job_result(&ctx, &result).await;
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
        super::input_status::complete_staged_input(ctx, &staged_input, &mut staged_result).await;
        playlist_download_result.partial |= staged_result.partial;
        playlist_download_result.download_err.append(&mut staged_result.errors);
        playlist_download_result.quality_rejections.append(&mut staged_result.quality_rejections);
        accepted_empty_clusters |= staged_result.accepted_empty_clusters;
        if let Some(staged_error) = staged_result.storage_error {
            playlist_download_result.download_err.push(staged_error);
        } else {
            let provider_groups = playlist.take_groups();
            let staged_groups = staged_result.source.take_groups();
            let merged_groups = apply_staged_overlay_groups(&input.name, clusters, provider_groups, staged_groups);
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

pub struct PlaylistProcessingContext<E: EventSink, M: MetadataUpdateSink = NoopMetadataSink> {
    pub client: reqwest::Client,
    pub run_id: PlaylistUpdateRunId,
    pub execution_order: PlaylistUpdateRunOrder,
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
    /// Completion precedence for this context's `run_id`, keyed by stable input ID.
    /// Fresh for every `exec_processing`; clones for parallel sources share it.
    pub(super) input_completions: Arc<Mutex<HashMap<u16, PlaylistUpdateState>>>,
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
    /// Optional request-local behavior for one manually selected input.
    pub input_refresh: Option<InputRefreshOverride>,
    pub(crate) library_update_mode: LibraryUpdateMode,
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

    fn acquisition(&self, input_id: u16) -> InputAcquisition {
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

#[cfg(test)]
mod pipeline_transparency_tests {
    use super::*;
    use shared::model::{ConfigInputOptionsDto, ConfigInputUpdateQualityDto};
    use tuliprox_core::model::ClusterUpdateAcceptance;

    fn input(input_type: InputType) -> ConfigInput {
        ConfigInput { id: 17, name: Arc::from("provider-a"), input_type, enabled: true, ..ConfigInput::default() }
    }

    fn cluster(telemetry: &PlaylistUpdateInputTelemetry, cluster: XtreamCluster) -> &PlaylistUpdateClusterTelemetry {
        telemetry.clusters.iter().find(|item| item.cluster == cluster).expect("configured cluster telemetry")
    }

    const fn completed_snapshot_context(
        effective_policy: InputRefreshPolicy,
        request_policy: Option<InputRefreshPolicy>,
        provider_cluster_count: usize,
    ) -> PersistedSnapshotContext {
        PersistedSnapshotContext {
            effective_policy,
            request_policy,
            technical_failure: false,
            storage_failure: false,
            provider_failure_evidence: ProviderFailureEvidence::None,
            provider_cluster_count,
        }
    }

    const fn failed_provider_snapshot_context(
        effective_policy: InputRefreshPolicy,
        request_policy: Option<InputRefreshPolicy>,
        provider_cluster_count: usize,
    ) -> PersistedSnapshotContext {
        PersistedSnapshotContext {
            effective_policy,
            request_policy,
            technical_failure: true,
            storage_failure: false,
            provider_failure_evidence: ProviderFailureEvidence::TechnicalFailure,
            provider_cluster_count,
        }
    }

    #[test]
    fn pipeline_transparency_telemetry_preserves_cache_rejection_and_force_decisions() {
        let input = input(InputType::Xtream);
        let fetch = PlaylistFetch::groups(Vec::new())
            .with_quality_rejections(vec![ClusterUpdateRejection {
                cluster: XtreamCluster::Video,
                current_count: 12_543,
                candidate_count: 217,
                threshold: 90,
                quality: 1,
            }])
            .with_force_updates(vec![ClusterForceUpdate {
                cluster: XtreamCluster::Series,
                candidate_count: 8_412,
                configured_threshold: 100,
            }]);

        let telemetry = input_telemetry_for_fetch(
            &input,
            InputRefreshPolicy::FORCE,
            PlaylistUpdateDataSource::Provider,
            &[XtreamCluster::Video, XtreamCluster::Series],
            Some(&fetch),
        );

        let live = cluster(&telemetry, XtreamCluster::Live);
        assert!(live.requested);
        assert_eq!(live.source, Some(PlaylistUpdateDataSource::Cache));
        assert_eq!(live.decision, None);

        let video = cluster(&telemetry, XtreamCluster::Video);
        assert_eq!(video.source, Some(PlaylistUpdateDataSource::Provider));
        assert_eq!(video.decision, Some(PlaylistUpdateClusterDecision::Rejected));
        assert_eq!(
            (video.baseline_count, video.candidate_count, video.active_count),
            (Some(12_543), Some(217), Some(12_543))
        );
        assert_eq!((video.threshold, video.quality), (Some(90), Some(1)));

        let series = cluster(&telemetry, XtreamCluster::Series);
        assert_eq!(series.source, Some(PlaylistUpdateDataSource::Provider));
        assert_eq!(series.decision, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!((series.candidate_count, series.active_count), (Some(8_412), None));
        assert_eq!(series.threshold, Some(100));

        let mut published = telemetry;
        confirm_published_cluster_counts(&mut published);
        assert_eq!(cluster(&published, XtreamCluster::Series).active_count, Some(8_412));
    }

    #[test]
    fn pipeline_transparency_telemetry_preserves_accepted_quality_and_bootstrap_facts() {
        let mut input = input(InputType::Xtream);
        input.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 90, vod: 90, series: 0 },
            ..ConfigInputOptionsDto::default()
        }));
        let fetch = PlaylistFetch::groups(Vec::new()).with_quality_acceptances(vec![
            ClusterUpdateAcceptance {
                cluster: XtreamCluster::Live,
                current_count: Some(12_543),
                candidate_count: 12_000,
                threshold: 90,
                quality: Some(95),
            },
            ClusterUpdateAcceptance {
                cluster: XtreamCluster::Video,
                current_count: None,
                candidate_count: 217,
                threshold: 90,
                quality: None,
            },
        ]);

        let mut telemetry = input_telemetry_for_fetch(
            &input,
            InputRefreshPolicy::NORMAL,
            PlaylistUpdateDataSource::Provider,
            &PIPELINE_TRANSPARENCY_CLUSTERS,
            Some(&fetch),
        );

        let live = cluster(&telemetry, XtreamCluster::Live);
        assert_eq!(live.decision, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!((live.baseline_count, live.candidate_count), (Some(12_543), Some(12_000)));
        assert_eq!((live.threshold, live.quality, live.active_count), (Some(90), Some(95), None));

        let video = cluster(&telemetry, XtreamCluster::Video);
        assert_eq!(video.decision, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!((video.baseline_count, video.candidate_count), (None, Some(217)));
        assert_eq!((video.threshold, video.quality, video.active_count), (Some(90), None, None));

        confirm_published_cluster_counts(&mut telemetry);
        assert_eq!(cluster(&telemetry, XtreamCluster::Live).active_count, Some(12_000));
        assert_eq!(cluster(&telemetry, XtreamCluster::Video).active_count, Some(217));
    }

    #[test]
    fn pipeline_transparency_accepted_quality_survives_fetch_error_without_active_claim() {
        let mut input = input(InputType::Xtream);
        input.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 90, ..ConfigInputUpdateQualityDto::default() },
            ..ConfigInputOptionsDto::default()
        }));
        let acceptance = ClusterUpdateAcceptance {
            cluster: XtreamCluster::Live,
            current_count: Some(100),
            candidate_count: 95,
            threshold: 90,
            quality: Some(95),
        };
        let fetch = PlaylistFetch::groups(Vec::new())
            .with_quality_acceptances(vec![acceptance])
            .with_errors(vec![TuliproxError::RepositoryXtream("post-quality publish failure".to_string())]);
        let telemetry = input_telemetry_for_fetch(
            &input,
            InputRefreshPolicy::NORMAL,
            PlaylistUpdateDataSource::Provider,
            &[XtreamCluster::Live],
            Some(&fetch),
        );
        let mut download_result = PlaylistDownloadResult::from(fetch).with_input_telemetry(telemetry);

        finalize_input_telemetry(&mut download_result, None);

        let telemetry = download_result.input_telemetry.as_ref().expect("input telemetry");
        let live = cluster(telemetry, XtreamCluster::Live);
        assert_eq!(live.decision, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!((live.baseline_count, live.candidate_count), (Some(100), Some(95)));
        assert_eq!((live.threshold, live.quality), (Some(90), Some(95)));
        assert_eq!(live.active_count, None);
        assert_eq!(download_result.download_err.len(), 1);
    }

    #[test]
    fn pipeline_transparency_telemetry_keeps_zero_threshold_quality_disabled() {
        let input = input(InputType::Xtream);
        let fetch = PlaylistFetch::groups(Vec::new());

        let telemetry = input_telemetry_for_fetch(
            &input,
            InputRefreshPolicy::NORMAL,
            PlaylistUpdateDataSource::Provider,
            &[XtreamCluster::Live],
            Some(&fetch),
        );

        let live = cluster(&telemetry, XtreamCluster::Live);
        assert_eq!(live.decision, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!(live.threshold, None);
        assert_eq!(live.quality, None);
        assert_eq!(live.active_count, None);
    }

    #[test]
    fn pipeline_transparency_telemetry_reports_non_cluster_cache_without_synthetic_clusters() {
        let telemetry = input_telemetry_for_fetch(
            &input(InputType::Plex),
            InputRefreshPolicy::NORMAL,
            PlaylistUpdateDataSource::Cache,
            &[],
            None,
        );

        assert_eq!(telemetry.refresh_policy, InputRefreshPolicy::NORMAL);
        assert_eq!(telemetry.source, Some(PlaylistUpdateDataSource::Cache));
        assert!(telemetry.clusters.is_empty());
    }

    #[test]
    fn pipeline_transparency_known_cluster_failure_keeps_quality_decision_and_neutralizes_overlay() {
        let input = input(InputType::Xtream);
        let acceptance = ClusterUpdateAcceptance {
            cluster: XtreamCluster::Series,
            current_count: Some(100),
            candidate_count: 95,
            threshold: 90,
            quality: Some(95),
        };
        let mut fetch = PlaylistFetch::groups(Vec::new()).with_quality_acceptances(vec![acceptance]);
        fetch.record_cluster_error(
            XtreamCluster::Series,
            TuliproxError::RepositoryXtream("publish failure".to_string()),
        );
        let telemetry = input_telemetry_for_fetch(
            &input,
            InputRefreshPolicy::NORMAL,
            PlaylistUpdateDataSource::Provider,
            &PIPELINE_TRANSPARENCY_CLUSTERS,
            Some(&fetch),
        );
        let mut result = PlaylistDownloadResult::from(fetch).with_input_telemetry(telemetry);
        finalize_input_telemetry(&mut result, None);
        let telemetry = result.input_telemetry.as_mut().unwrap();
        let shows = cluster(telemetry, XtreamCluster::Series);
        assert_eq!(shows.decision, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!(shows.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
        assert_eq!(shows.active_count, None);
        let context = failed_provider_snapshot_context(InputRefreshPolicy::NORMAL, None, 3);
        let snapshot = persisted_cluster_snapshot(shows, context);
        assert_eq!(snapshot.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
        assert_eq!(snapshot.quality.unwrap().decision, PersistedPlaylistUpdateQualityDecision::Accepted);
        neutralize_overlaid_cluster_facts(telemetry, ClusterFlags::Series);
        let overlaid = cluster(telemetry, XtreamCluster::Series);
        assert_eq!(overlaid.technical_state, None);
        assert_eq!(persisted_cluster_snapshot(overlaid, context).technical_state, None);
    }

    #[test]
    fn pipeline_transparency_telemetry_types_complete_provider_failure_without_guessing_metrics() {
        let mut input = input(InputType::Xtream);
        input.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
            skip_vod: true,
            skip_series: true,
            ..ConfigInputOptionsDto::default()
        }));
        let fetch = PlaylistFetch::failed(TuliproxError::Download("provider unavailable".to_string()));

        let telemetry = input_telemetry_for_fetch(
            &input,
            InputRefreshPolicy::REFRESH,
            PlaylistUpdateDataSource::Provider,
            &[XtreamCluster::Live],
            Some(&fetch),
        );

        assert_eq!(telemetry.refresh_policy, InputRefreshPolicy::REFRESH);
        let live = cluster(&telemetry, XtreamCluster::Live);
        assert!(live.requested);
        assert_eq!(live.source, Some(PlaylistUpdateDataSource::Provider));
        assert_eq!(live.decision, Some(PlaylistUpdateClusterDecision::TechnicalError));
        assert!(live.baseline_count.is_none());
        assert!(live.candidate_count.is_none());
        assert!(live.active_count.is_none());
        assert!(live.quality.is_none());
    }

    #[test]
    fn pipeline_transparency_telemetry_does_not_assign_an_unscoped_error_to_multiple_clusters() {
        let input = input(InputType::Stalker);
        let fetch = PlaylistFetch::failed(TuliproxError::Download("provider unavailable".to_string()));

        let telemetry = input_telemetry_for_fetch(
            &input,
            InputRefreshPolicy::NORMAL,
            PlaylistUpdateDataSource::Provider,
            &PIPELINE_TRANSPARENCY_CLUSTERS,
            Some(&fetch),
        );

        assert!(telemetry.clusters.iter().all(|item| item.requested && item.decision.is_none()));
    }

    #[test]
    fn pipeline_transparency_telemetry_does_not_infer_an_unreported_quality_acceptance() {
        let mut input = input(InputType::Xtream);
        input.options = Some(ConfigInputOptions::from(&ConfigInputOptionsDto {
            skip_vod: true,
            update_quality: ConfigInputUpdateQualityDto { live: 90, ..ConfigInputUpdateQualityDto::default() },
            ..ConfigInputOptionsDto::default()
        }));
        let fetch = PlaylistFetch::groups(Vec::new());

        let telemetry = input_telemetry_for_fetch(
            &input,
            InputRefreshPolicy::NORMAL,
            PlaylistUpdateDataSource::Provider,
            &[XtreamCluster::Live],
            Some(&fetch),
        );

        let live = cluster(&telemetry, XtreamCluster::Live);
        assert_eq!(live.threshold, Some(90));
        assert_eq!(live.decision, None, "an accepted quality decision was not retained by the provider result");
        let video = cluster(&telemetry, XtreamCluster::Video);
        assert!(!video.requested);
        assert_eq!(video.source, None);
        assert_eq!(video.decision, None);
    }

    #[test]
    fn persisted_cluster_snapshot_preserves_quality_facts_without_merging_technical_state() {
        let accepted = PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Live,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Provider),
            baseline_count: Some(1_000),
            candidate_count: Some(950),
            active_count: Some(950),
            threshold: Some(90),
            quality: Some(95),
            decision: Some(PlaylistUpdateClusterDecision::Accepted),
            technical_state: None,
        };

        let completed = persisted_cluster_snapshot(
            &accepted,
            completed_snapshot_context(InputRefreshPolicy::NORMAL, Some(InputRefreshPolicy::REFRESH), 1),
        );
        assert_eq!(completed.policy, Some(InputRefreshPolicy::REFRESH));
        assert_eq!(completed.source, Some(PlaylistUpdateDataSource::Provider));
        assert_eq!(completed.quality_guard_threshold, None, "an evaluation already owns its threshold");
        assert_eq!(
            completed.quality,
            Some(PersistedPlaylistUpdateQualitySnapshot {
                threshold: 90,
                baseline_count: Some(1_000),
                candidate_count: Some(950),
                achieved_quality: Some(95),
                decision: PersistedPlaylistUpdateQualityDecision::Accepted,
            })
        );
        assert_eq!(completed.active_count, Some(950));
        assert_eq!(completed.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));

        let mut bootstrap = accepted.clone();
        bootstrap.baseline_count = None;
        bootstrap.candidate_count = Some(42);
        bootstrap.active_count = Some(42);
        bootstrap.quality = None;
        let bootstrap =
            persisted_cluster_snapshot(&bootstrap, completed_snapshot_context(InputRefreshPolicy::NORMAL, None, 1));
        assert_eq!(bootstrap.policy, None, "an automatic run must not acquire a default policy");
        assert_eq!(bootstrap.quality_guard_threshold, None, "bootstrap evaluation already owns its threshold");
        assert_eq!(
            bootstrap.quality,
            Some(PersistedPlaylistUpdateQualitySnapshot {
                threshold: 90,
                baseline_count: None,
                candidate_count: Some(42),
                achieved_quality: None,
                decision: PersistedPlaylistUpdateQualityDecision::Accepted,
            }),
            "bootstrap acceptance must not invent achieved Quality"
        );

        let mut rejected = accepted.clone();
        rejected.candidate_count = Some(217);
        rejected.active_count = Some(1_000);
        rejected.quality = Some(21);
        rejected.decision = Some(PlaylistUpdateClusterDecision::Rejected);
        let rejected =
            persisted_cluster_snapshot(&rejected, completed_snapshot_context(InputRefreshPolicy::NORMAL, None, 1));
        assert_eq!(
            rejected.quality.map(|quality| quality.decision),
            Some(PersistedPlaylistUpdateQualityDecision::Rejected)
        );
        assert_eq!(rejected.active_count, Some(1_000), "only the typed active baseline is retained");
        assert_eq!(rejected.quality_guard_threshold, None, "a rejection evaluation already owns its threshold");
        assert_eq!(rejected.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));

        let mut accepted_then_failed = accepted;
        accepted_then_failed.active_count = None;
        let accepted_then_failed = persisted_cluster_snapshot(
            &accepted_then_failed,
            PersistedSnapshotContext {
                effective_policy: InputRefreshPolicy::NORMAL,
                request_policy: None,
                technical_failure: true,
                storage_failure: true,
                provider_failure_evidence: ProviderFailureEvidence::None,
                provider_cluster_count: 1,
            },
        );
        assert_eq!(
            accepted_then_failed.quality.map(|quality| quality.decision),
            Some(PersistedPlaylistUpdateQualityDecision::Accepted)
        );
        assert_eq!(accepted_then_failed.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
        assert_eq!(accepted_then_failed.active_count, None);
    }

    #[test]
    fn persisted_cluster_snapshot_keeps_policy_source_and_disabled_guard_independent() {
        let forced = PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Video,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Provider),
            baseline_count: None,
            candidate_count: Some(73),
            active_count: Some(73),
            threshold: Some(95),
            quality: None,
            decision: Some(PlaylistUpdateClusterDecision::Accepted),
            technical_state: None,
        };

        let forced = persisted_cluster_snapshot(
            &forced,
            completed_snapshot_context(InputRefreshPolicy::FORCE, Some(InputRefreshPolicy::FORCE), 1),
        );
        assert_eq!(forced.policy, Some(InputRefreshPolicy::FORCE));
        assert_eq!(forced.source, Some(PlaylistUpdateDataSource::Provider));
        assert_eq!(forced.quality_guard_threshold, Some(95));
        assert_eq!(forced.quality, None, "a Force bypass is not a Quality evaluation");

        let disabled = PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Series,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Provider),
            baseline_count: None,
            candidate_count: Some(12),
            active_count: Some(12),
            threshold: None,
            quality: None,
            decision: Some(PlaylistUpdateClusterDecision::Accepted),
            technical_state: None,
        };
        let disabled = persisted_cluster_snapshot(
            &disabled,
            completed_snapshot_context(InputRefreshPolicy::FORCE, Some(InputRefreshPolicy::FORCE), 1),
        );
        assert_eq!(disabled.quality_guard_threshold, Some(0));
        assert_eq!(disabled.quality, None, "threshold zero stays disabled even during Force");

        let cached = PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Live,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Cache),
            baseline_count: None,
            candidate_count: None,
            active_count: None,
            threshold: Some(90),
            quality: None,
            decision: None,
            technical_state: None,
        };
        let cached = persisted_cluster_snapshot(
            &cached,
            completed_snapshot_context(InputRefreshPolicy::REFRESH, Some(InputRefreshPolicy::REFRESH), 0),
        );
        assert_eq!(cached.policy, Some(InputRefreshPolicy::REFRESH));
        assert_eq!(cached.quality_guard_threshold, Some(90));
        assert_eq!(
            cached.source,
            Some(PlaylistUpdateDataSource::Cache),
            "the actual source remains stronger than request policy"
        );
    }

    #[test]
    fn persisted_cluster_snapshot_final_status_boundary_replaces_provider_with_cache_without_history() {
        let temp = tempfile::tempdir().expect("temporary status directory");
        let mut status = input_cache::InputStatus::default();
        status.clusters.insert(
            XtreamCluster::Live.as_ref().to_string(),
            input_cache::ClusterStatus { status: input_cache::ClusterState::Ok, timestamp: 17, last_update: None },
        );
        let provider_telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::REFRESH,
            source: None,
            clusters: vec![PlaylistUpdateClusterTelemetry {
                cluster: XtreamCluster::Live,
                requested: true,
                source: Some(PlaylistUpdateDataSource::Provider),
                baseline_count: Some(100),
                candidate_count: Some(95),
                active_count: Some(95),
                threshold: Some(90),
                quality: Some(95),
                decision: Some(PlaylistUpdateClusterDecision::Accepted),
                technical_state: None,
            }],
        };
        let mut provider_result = PlaylistDownloadResult::new(Vec::new(), Vec::new(), false, false)
            .with_input_telemetry(provider_telemetry)
            .with_pending_status(PendingInputStatusUpdate {
                storage_path: temp.path().to_path_buf(),
                status,
                status_changed: true,
                cluster_status_source: ClusterStatusSource::PerCluster,
            });

        persist_finalized_cluster_snapshots(&mut provider_result, Some(InputRefreshPolicy::REFRESH), None);

        let provider_status = input_cache::load_input_status(temp.path());
        let provider_snapshot = provider_status.clusters["live"].last_update.expect("provider snapshot");
        assert_eq!(provider_snapshot.source, Some(PlaylistUpdateDataSource::Provider));
        assert!(provider_snapshot.quality.is_some());

        let cache_telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::NORMAL,
            source: None,
            clusters: vec![PlaylistUpdateClusterTelemetry {
                cluster: XtreamCluster::Live,
                requested: true,
                source: Some(PlaylistUpdateDataSource::Cache),
                baseline_count: None,
                candidate_count: None,
                active_count: None,
                threshold: Some(90),
                quality: None,
                decision: None,
                technical_state: None,
            }],
        };
        let mut cache_result = PlaylistDownloadResult::new(Vec::new(), Vec::new(), true, false)
            .with_input_telemetry(cache_telemetry)
            .with_pending_status(PendingInputStatusUpdate {
                storage_path: temp.path().to_path_buf(),
                status: provider_status,
                status_changed: false,
                cluster_status_source: ClusterStatusSource::PerCluster,
            });

        persist_finalized_cluster_snapshots(&mut cache_result, Some(InputRefreshPolicy::NORMAL), None);

        let cached_status = input_cache::load_input_status(temp.path());
        assert_eq!(cached_status.clusters.len(), 1);
        let cached_snapshot = cached_status.clusters["live"].last_update.expect("replacement cache snapshot");
        assert_eq!(cached_snapshot.policy, Some(InputRefreshPolicy::NORMAL));
        assert_eq!(cached_snapshot.source, Some(PlaylistUpdateDataSource::Cache));
        assert_eq!(cached_snapshot.quality_guard_threshold, Some(90));
        assert_eq!(cached_snapshot.quality, None);
        assert_eq!(cached_snapshot.active_count, None);
        assert_eq!(cached_snapshot.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));
        let encoded =
            std::fs::read_to_string(temp.path().join(input_cache::STATUS_FILE)).expect("persisted status JSON");
        assert_eq!(encoded.matches(r#""last_update""#).count(), 1);
        assert!(!encoded.contains("history"));
    }

    #[test]
    fn persisted_cluster_snapshot_discards_unproven_staged_overlay_facts() {
        let mut telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::FORCE,
            source: None,
            clusters: vec![PlaylistUpdateClusterTelemetry {
                cluster: XtreamCluster::Video,
                requested: true,
                source: Some(PlaylistUpdateDataSource::Provider),
                baseline_count: Some(100),
                candidate_count: Some(95),
                active_count: Some(95),
                threshold: Some(90),
                quality: Some(95),
                decision: Some(PlaylistUpdateClusterDecision::Accepted),
                technical_state: None,
            }],
        };
        neutralize_overlaid_cluster_facts(&mut telemetry, ClusterFlags::Vod);

        let snapshot = persisted_cluster_snapshot(
            cluster(&telemetry, XtreamCluster::Video),
            completed_snapshot_context(telemetry.refresh_policy, Some(InputRefreshPolicy::FORCE), 0),
        );

        assert_eq!(snapshot, PersistedPlaylistUpdateClusterSnapshot::default());
    }

    #[test]
    fn persisted_cluster_snapshot_leaves_unscoped_multi_cluster_failure_unknown() {
        let cluster = PlaylistUpdateClusterTelemetry {
            cluster: XtreamCluster::Live,
            requested: true,
            source: Some(PlaylistUpdateDataSource::Provider),
            baseline_count: Some(100),
            candidate_count: Some(95),
            active_count: None,
            threshold: Some(90),
            quality: Some(95),
            decision: Some(PlaylistUpdateClusterDecision::Accepted),
            technical_state: None,
        };

        let snapshot =
            persisted_cluster_snapshot(&cluster, failed_provider_snapshot_context(InputRefreshPolicy::NORMAL, None, 2));

        assert_eq!(snapshot.technical_state, None);
        assert_eq!(
            snapshot.quality.map(|quality| quality.decision),
            Some(PersistedPlaylistUpdateQualityDecision::Accepted)
        );

        let unscoped_ancillary_failure = persisted_cluster_snapshot(
            &cluster,
            PersistedSnapshotContext {
                effective_policy: InputRefreshPolicy::NORMAL,
                request_policy: None,
                technical_failure: true,
                storage_failure: false,
                provider_failure_evidence: ProviderFailureEvidence::None,
                provider_cluster_count: 1,
            },
        );
        assert_eq!(
            unscoped_ancillary_failure.technical_state, None,
            "an input-wide ancillary failure is not invented as a cluster-scoped provider failure"
        );
    }
}
