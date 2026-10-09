use super::{
    telemetry::{input_telemetry_for_fetch, is_cluster_input},
    PendingInputStatusUpdate, PlaylistDownloadResult, PlaylistProcessingContext,
};
use crate::metadata_sink::MetadataUpdateSink;
use log::info;
use shared::{
    error::TuliproxError,
    model::{
        ClusterFlags, EventMessage, EventSink, InputRefreshPolicy, InputType, PlaylistGroup, PlaylistUpdateDataSource,
        PlaylistUpdateInputTelemetry, PlaylistUpdateProgressEvent, XtreamCluster,
    },
};
use std::collections::HashSet;
use tuliprox_core::model::{ClusterForceUpdate, ConfigInput, ConfigInputFlags, ProcessTargets};
use tuliprox_iptv::{provider::PlaylistFetch, xtream};
use tuliprox_repository::PlaylistSource;

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

#[derive(Clone, Copy)]
pub(super) enum ClusterStatusSource {
    PerCluster,
    Default,
}

#[derive(Clone, Copy, Default)]
pub(super) enum ProviderFailureEvidence {
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

    pub(super) fn with_input_telemetry(mut self, input_telemetry: PlaylistUpdateInputTelemetry) -> Self {
        self.input_telemetry = Some(input_telemetry);
        self
    }

    pub(super) fn with_pending_status(mut self, pending_status: PendingInputStatusUpdate) -> Self {
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

pub(super) fn cached_playlist_download_result(
    input: &ConfigInput,
    refresh_policy: InputRefreshPolicy,
) -> PlaylistDownloadResult {
    PlaylistDownloadResult::new(vec![], vec![], true, false).with_input_telemetry(input_telemetry_for_fetch(
        input,
        refresh_policy,
        PlaylistUpdateDataSource::Cache,
        &[],
        None,
    ))
}

/// Loads an input already acquired in this run without reporting a second acquisition.
pub(super) fn in_run_reuse_playlist_download_result() -> PlaylistDownloadResult {
    PlaylistDownloadResult::new(vec![], vec![], true, false)
}

pub(super) fn forced_empty_cluster_flags(force_updates: &[ClusterForceUpdate]) -> ClusterFlags {
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

pub(super) fn report_forced_update_request<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
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
    ctx.events.emit(EventMessage::PlaylistUpdateProgress(
        PlaylistUpdateProgressEvent::for_run_input(
            ctx.run_id.clone(),
            ctx.execution_order,
            input.id,
            input.name.to_string(),
            message,
        )
        .with_detail(shared::model::PlaylistUpdateProgressDetail::ForcedUpdateRequested),
    ));
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
