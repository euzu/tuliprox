#![allow(clippy::wildcard_imports)]

use super::{
    fetch_outcome::{apply_playlist_fetch_outcome, report_force_updates, CacheStatusScope},
    *,
};

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
struct PersistedSnapshotContext {
    effective_policy: InputRefreshPolicy,
    request_policy: Option<InputRefreshPolicy>,
    technical_failure: bool,
    storage_failure: bool,
    provider_failure_evidence: ProviderFailureEvidence,
    provider_cluster_count: usize,
}

/// Staged groups resolved against the provider groups they overlay.
///
/// Both directions are kept in sync, so one staged group can never overlay two provider categories
/// and one provider category can never receive two staged groups.
struct StagedOverlayAssignment {
    staged_by_provider: Vec<Option<usize>>,
    provider_by_staged: Vec<Option<usize>>,
    has_stream_id_evidence: Vec<bool>,
}

/// One provider category a staged group shares streams with.
struct StreamOverlap {
    hits: usize,
    staged_idx: usize,
    provider_idx: usize,
}

/// Cluster-scoped category ids of a merged playlist.
///
/// Provider category ids are reserved up front because they are authoritative: an overlaid group
/// keeps them, and a staged group that introduces a new category must not take them over.
#[derive(Default)]
struct GroupCategoryIds {
    used: HashMap<XtreamCluster, HashSet<u32>>,
    next_free: HashMap<XtreamCluster, u32>,
}

#[derive(Clone, Copy)]
struct PlaylistUpdateExecutionRef<'a> {
    run_id: &'a PlaylistUpdateRunId,
    execution_order: PlaylistUpdateRunOrder,
}

// The same completion contract applies to a source job (including EPG errors)
// and an indirect staged download, which has no separate source job.
struct InputCompletionFacts {
    job_state: InputJobState,
    had_errors: bool,
    had_quality_rejections: bool,
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

#[cfg(test)]
mod pipeline_transparency_tests;

mod acquisition;
mod jobs;
mod overlay;
mod status;
mod telemetry;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(crate) use acquisition::{download_input, invalidate_input_cache_status, load_cached_input_playlist};
pub(super) use jobs::report_input_completion;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(crate) use jobs::{
    create_broadcast_callback, create_input_stat, download_input_epg, panicked_input_job, process_input_job,
    process_input_job_inner, process_source, process_sources, report_input_job_completion, InputDownloadResult,
    InputJobResult, InputJobState,
};
pub(crate) use overlay::{apply_staged_overlay_groups, should_apply_staged_overlay};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(crate) use status::{
    cluster_selected, collect_effective_skip_clusters, filter_skipped_clusters_from_source, is_input_enabled,
    with_sequential_group,
};
use status::{ClusterStatusSource, ProviderFailureEvidence};
pub(crate) use telemetry::neutralize_overlaid_cluster_facts;
pub(super) use telemetry::{cluster_is_configured, PIPELINE_TRANSPARENCY_CLUSTERS};
#[cfg(test)]
use telemetry::{confirm_published_cluster_counts, persisted_cluster_snapshot};
