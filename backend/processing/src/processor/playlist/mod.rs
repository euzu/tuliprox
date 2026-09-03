use super::providers::{LibraryProvider, PlexProvider, StalkerProvider, XmltvEpgProvider};
use crate::{
    fetched_playlist::FetchedPlaylist,
    input_cache,
    metadata_sink::{MetadataUpdateSink, NoopMetadataSink},
    parser::xmltv::{flatten_tvguide, merge_epg_trees, EpgMergeAccumulator, TVGuide},
    playlist_watch::{process_group_watch, process_target_groups_watch},
    processor::{
        epg::{clear_invalid_live_epg_ids, process_playlist_epg, retain_epg_referenced_by_groups},
        sort::sort_playlist,
        trakt::process_trakt_categories_for_target,
        xtream_series::playlist_resolve_series,
        xtream_vod::playlist_resolve_vod,
        StalkerRefreshMode,
    },
};
use futures::{FutureExt, StreamExt};
use indexmap::IndexMap;
use log::{debug, error, info, log_enabled, warn, Level};
use path_clean::PathClean;
use shared::{
    concat_string,
    defaults::{default_as_default, default_probe_delay_secs, default_probe_live_interval},
    error::{get_errors_notify_message, TuliproxError},
    foundation::{get_field_value, set_field_value, Filter, ValueAccessor, ValueProvider},
    model::{
        ClusterFlags, ConfigTargetOptions, CounterModifier, EventMessage, EventSink, FieldGet, FieldSet, InputStats,
        InputType, MappingStage, PipelineStats, PlaylistGroup, PlaylistItem, PlaylistItemType, PlaylistStats,
        PlaylistUpdateProgressEvent, PlaylistUpdateState, PlaylistUpdateSummary, ProviderFetchFailure, SourceStats,
        StreamProperties, TargetStats, UUIDType, WatchDisabled, WatchDisabledReason, WatchUnmatched, XtreamCluster,
    },
    utils::{create_alias_uuid, interner_gc, sanitize_sensitive_info, Internable},
};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::{
    sync::{watch, Mutex, OwnedRwLockWriteGuard, RwLock},
    task::JoinSet,
};
use tuliprox_core::{
    model::{
        is_valid, retain_filtered_playlist, AppConfig, ClusterUpdateRejection, CompiledMapping, ConfigFavourites,
        ConfigInput, ConfigInputFlags, ConfigInputOptions, ConfigRename, ConfigTarget, Epg, FilterOutcome,
        MappingProgram, ProcessTargets, ProviderIdType, ResolveReason, ReverseProxyDisabledHeaderConfig,
        TransformStage, UpdateGuard, UpdateTask,
    },
    utils::{debug_if_enabled, log_memory_snapshot, trace_if_enabled, StepMeasure, StepMeasureCallback},
};
use tuliprox_iptv::{
    epg::{CountingEpgSink, EpgFetchRequest, EpgProvider},
    error::ProviderErrorKind,
    provider::{
        BatchContainerProvider, M3uProvider, PlaylistFetch, PlaylistFetchRequest, PlaylistProvider,
        UnsupportedProvider, XtreamProvider,
    },
    xtream,
};
use tuliprox_repository::{
    load_input_playlist, persist_input_playlist, persist_playlist, CategoryKey, MemoryPlaylistSource, PlaylistSource,
    PlaylistStorageState,
};
use tuliprox_session::ActiveProviderManager;

const PLAYLIST_UPDATE_MAX_DURATION_SECS: u64 = 3600;
const MAX_CONCURRENT_TARGET_FINALIZERS: usize = 2;

mod fetch_outcome;
mod ingest;
mod target;
mod transform;

pub use self::{ingest::*, target::*, transform::*};

/// Work the composition root runs once the playlist lock is held, before the
/// update proper starts.
///
/// This was an `Option<Arc<AppState>>` used for exactly one call. Passing the
/// call instead of the state keeps `processing` from naming the server state.
///
/// It was then an
/// `Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>`:
/// two layers of erasure and a heap allocation for a future that is awaited
/// exactly once per update, and every call site had to spell out both
/// coercions. As a trait it is one type parameter, monomorphised, with the
/// future returned by value.
pub trait UpdateBootstrap: Send + Sync + 'static {
    fn run(&self) -> impl Future<Output = ()> + Send;
}

impl<F, Fut> UpdateBootstrap for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send,
{
    fn run(&self) -> impl Future<Output = ()> + Send { self() }
}

/// The bootstrap type parameter of a run that has no bootstrap.
///
/// A function pointer rather than a unit struct: it satisfies the blanket
/// `Fn` impl above, so no second impl - and no coherence problem - is needed.
/// A value of this type is never constructed; the field is always `None`.
pub type NoBootstrap = fn() -> std::future::Ready<()>;

/// Everything one playlist update run needs.
///
/// `exec_processing` took twelve positional arguments, seven of them
/// `Option<_>`, so a call site was a wall of `None`s and `Some(..)`s in which
/// the reader had to count commas to work out which knob was being set - and
/// the compiler could not catch two same-typed arguments swapped.
///
/// Four of the twelve are always present, so they are constructor arguments.
/// The rest are optional in fact as well as in type, and a call site names the
/// ones it actually sets.
pub struct ProcessingRun<
    E: EventSink + Clone + 'static,
    B: UpdateBootstrap = NoBootstrap,
    M: MetadataUpdateSink = NoopMetadataSink,
> {
    client: reqwest::Client,
    app_config: Arc<AppConfig>,
    targets: Arc<ProcessTargets>,
    events: E,
    bootstrap: Option<B>,
    playlist_state: Option<Arc<PlaylistStorageState>>,
    update_guard: Option<UpdateGuard>,
    disabled_headers: Option<ReverseProxyDisabledHeaderConfig>,
    provider_manager: Option<Arc<ActiveProviderManager>>,
    metadata_manager: Option<Arc<M>>,
    pre_processed_inputs: Option<HashSet<Arc<str>>>,
    acquired_permit: Option<tuliprox_core::model::UpdateGuardPermit>,
}

impl<E: EventSink + Clone + 'static> ProcessingRun<E, NoBootstrap, NoopMetadataSink> {
    pub fn new(client: reqwest::Client, app_config: Arc<AppConfig>, targets: Arc<ProcessTargets>, events: E) -> Self {
        Self {
            client,
            app_config,
            targets,
            events,
            bootstrap: None,
            playlist_state: None,
            update_guard: None,
            disabled_headers: None,
            provider_manager: None,
            metadata_manager: None,
            pre_processed_inputs: None,
            acquired_permit: None,
        }
    }
}

impl<E: EventSink + Clone + 'static, B: UpdateBootstrap, M: MetadataUpdateSink> ProcessingRun<E, B, M> {
    /// Work the composition root runs once the lock is held, before the update
    /// proper starts.
    ///
    /// Changes the run's bootstrap type, so it rebuilds rather than mutates.
    #[must_use]
    pub fn with_bootstrap<B2: UpdateBootstrap>(self, bootstrap: B2) -> ProcessingRun<E, B2, M> {
        ProcessingRun {
            client: self.client,
            app_config: self.app_config,
            targets: self.targets,
            events: self.events,
            bootstrap: Some(bootstrap),
            playlist_state: self.playlist_state,
            update_guard: self.update_guard,
            disabled_headers: self.disabled_headers,
            provider_manager: self.provider_manager,
            metadata_manager: self.metadata_manager,
            pre_processed_inputs: self.pre_processed_inputs,
            acquired_permit: self.acquired_permit,
        }
    }

    #[must_use]
    pub fn with_playlist_state(mut self, state: impl Into<Option<Arc<PlaylistStorageState>>>) -> Self {
        self.playlist_state = state.into();
        self
    }

    /// The lock this run acquires. Ignored when an already-acquired permit is
    /// supplied via [`Self::with_acquired_permit`].
    #[must_use]
    pub fn with_update_guard(mut self, guard: impl Into<Option<UpdateGuard>>) -> Self {
        self.update_guard = guard.into();
        self
    }

    #[must_use]
    pub fn with_disabled_headers(mut self, headers: impl Into<Option<ReverseProxyDisabledHeaderConfig>>) -> Self {
        self.disabled_headers = headers.into();
        self
    }

    #[must_use]
    pub fn with_provider_manager(mut self, manager: impl Into<Option<Arc<ActiveProviderManager>>>) -> Self {
        self.provider_manager = manager.into();
        self
    }

    /// The background metadata worker.
    ///
    /// Changes the run's sink type, so it rebuilds rather than mutates.
    #[must_use]
    pub fn with_metadata_manager<M2: MetadataUpdateSink>(self, manager: Arc<M2>) -> ProcessingRun<E, B, M2> {
        ProcessingRun {
            client: self.client,
            app_config: self.app_config,
            targets: self.targets,
            events: self.events,
            bootstrap: self.bootstrap,
            playlist_state: self.playlist_state,
            update_guard: self.update_guard,
            disabled_headers: self.disabled_headers,
            provider_manager: self.provider_manager,
            metadata_manager: Some(manager),
            pre_processed_inputs: self.pre_processed_inputs,
            acquired_permit: self.acquired_permit,
        }
    }

    // Always built with the default hasher here; generalising would buy nothing.
    #[allow(clippy::implicit_hasher)]
    #[must_use]
    pub fn with_pre_processed_inputs(mut self, inputs: impl Into<Option<HashSet<Arc<str>>>>) -> Self {
        self.pre_processed_inputs = inputs.into();
        self
    }

    /// A playlist lock the caller already holds. Takes precedence over
    /// [`Self::with_update_guard`], which would otherwise acquire a second one.
    #[must_use]
    pub fn with_acquired_permit(mut self, permit: impl Into<Option<tuliprox_core::model::UpdateGuardPermit>>) -> Self {
        self.acquired_permit = permit.into();
        self
    }
}

#[allow(clippy::too_many_lines)]
pub async fn exec_processing<E: EventSink + Clone + 'static, B: UpdateBootstrap, M: MetadataUpdateSink>(
    run: ProcessingRun<E, B, M>,
) {
    let ProcessingRun {
        client,
        app_config,
        targets,
        events,
        bootstrap,
        playlist_state,
        update_guard,
        disabled_headers,
        provider_manager,
        metadata_manager,
        pre_processed_inputs,
        acquired_permit,
    } = run;

    let max_update_duration = Duration::from_secs(PLAYLIST_UPDATE_MAX_DURATION_SECS);
    let playlist_guard = if let Some(permit) = acquired_permit {
        Some(permit)
    } else if let Some(guard) = &update_guard {
        if let Some(permit) = guard.acquire_playlist_lock().await {
            Some(permit)
        } else {
            warn!("Playlist update lock is closed; update skipped.");
            events.emit(EventMessage::PlaylistUpdate(PlaylistUpdateSummary::state_only(
                shared::model::PlaylistUpdateState::Failure,
            )));
            return;
        }
    } else {
        None
    };

    if playlist_guard.is_some() {
        if let Some(bootstrap) = bootstrap.as_ref() {
            if tokio::time::timeout(max_update_duration, bootstrap.run()).await.is_err() {
                error!(
                    "Playlist update bootstrap timed out after {PLAYLIST_UPDATE_MAX_DURATION_SECS} secs while holding playlist lock",
                );
                events.emit(EventMessage::PlaylistUpdate(PlaylistUpdateSummary::state_only(
                    shared::model::PlaylistUpdateState::Failure,
                )));
                return;
            }
        }
    }

    // Pause background metadata/probe tasks for the full update lifecycle.
    let _background_pause_guard = if let Some(manager) = metadata_manager.as_ref() {
        Some(manager.acquire_update_pause_guard().await)
    } else {
        None
    };

    info!("🌷 Update process started.");

    log_memory_snapshot("exec_processing start");

    // Initialize Context
    let ctx = PlaylistProcessingContext {
        client,
        config: app_config.clone(),
        user_targets: targets.clone(),
        events: events.clone(),
        playlist_state: playlist_state.clone(),
        processed_inputs: Arc::new(Mutex::new(HashSet::new())),
        input_locks: Arc::new(Mutex::new(HashMap::new())),
        disabled_headers,
        provider_manager,
        metadata_manager,
        pre_processed_inputs: pre_processed_inputs.map(Arc::new),
        stalker_refresh_mode: if app_config.config.load().process_parallel {
            StalkerRefreshMode::Parallel
        } else if update_guard.is_some() {
            StalkerRefreshMode::ServerSlice
        } else {
            StalkerRefreshMode::Complete
        },
        partial_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        had_quality_rejections: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let start_time = Instant::now();
    let process_result =
        tokio::time::timeout(max_update_duration, std::panic::AssertUnwindSafe(process_sources(&ctx)).catch_unwind())
            .await;
    let (stats, errors) = match process_result {
        Ok(Ok((stats, errors))) => (stats, errors),
        Ok(Err(_)) => {
            error!("Playlist processing panicked");
            events.emit(EventMessage::PlaylistUpdate(PlaylistUpdateSummary::state_only(
                shared::model::PlaylistUpdateState::Failure,
            )));
            return;
        }
        Err(_) => {
            error!(
                "Playlist processing timed out after {PLAYLIST_UPDATE_MAX_DURATION_SECS} secs while holding playlist lock",
            );
            events.emit(EventMessage::PlaylistUpdate(PlaylistUpdateSummary::state_only(
                shared::model::PlaylistUpdateState::Failure,
            )));
            return;
        }
    };
    log_memory_snapshot("exec_processing after_process_sources");

    // Keep the update lock only for the critical processing section.
    drop(playlist_guard);
    debug!("Released playlist update lock; dispatching notifications and events");

    // log errors
    for err in &errors {
        error!("{}", err.message());
    }

    if !stats.is_empty() {
        if let Ok(stats_msg) = serde_json::to_string(&stats) {
            info!("stats: {stats_msg}");
        }
    }

    // One event for the whole run, carrying both the outcome and what it
    // did. These used to be two independent messages - the statistics went
    // straight to the notification layer, the outcome went to the bus - and
    // because both resolve to `playlist.update.completed`, a successful
    // refresh notified twice. Subscribers now get one event with everything,
    // and the bridge renders the single message from it.
    let error = get_errors_notify_message!(errors, 255);
    let outcome = PlaylistRunSignals {
        has_error: error.is_some(),
        has_pending_stalker_refresh: ctx.partial_refresh.load(std::sync::atomic::Ordering::Acquire),
        has_quality_rejections: ctx.had_quality_rejections.load(std::sync::atomic::Ordering::Acquire),
    }
    .state();
    events.emit(EventMessage::PlaylistUpdate(PlaylistUpdateSummary { state: outcome, stats, error }));

    let elapsed = start_time.elapsed().as_secs();
    let update_finished_message = format!("🌷 Update process finished! Took {elapsed} secs.");

    events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
        target: "Playlist Update".to_string(),
        message: update_finished_message.clone(),
    }));
    log_memory_snapshot("exec_processing before_interner_gc");
    debug!("StringInterner GC removed {} strings", interner_gc());
    log_memory_snapshot("exec_processing after_interner_gc");
    //trim_allocator_after_update();

    info!("{update_finished_message}");
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PlaylistRunSignals {
    pub(crate) has_error: bool,
    pub(crate) has_pending_stalker_refresh: bool,
    pub(crate) has_quality_rejections: bool,
}

impl PlaylistRunSignals {
    pub(crate) const fn state(self) -> PlaylistUpdateState {
        if self.has_error {
            PlaylistUpdateState::Failure
        } else if self.has_pending_stalker_refresh || self.has_quality_rejections {
            PlaylistUpdateState::Partial
        } else {
            PlaylistUpdateState::Success
        }
    }
}

#[cfg(test)]
mod tests;
