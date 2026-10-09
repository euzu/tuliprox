use super::EventKind;
use crate::model::{
    auth_audit::{AuthAuditEvent, AuthAuditOutcome},
    notification::{registry, EventId, Severity},
    stats::SourceStats,
    ActiveUserConnectionChange, ConfigReloadFailure, ConfigType, ConnectionDenied, DiskAlert, LibraryScanProgressEvent,
    LibraryScanSummaryStatus, MetadataUpdateFailure, MsgKind, NotificationDeadLetter, Permission,
    PlaylistGroupsChanged, PlaylistUpdateProgressEvent, PlaylistUpdateRunId, PlaylistUpdateRunOrder,
    PlaylistUpdateState, ProviderAccountEvent, ProviderAccountState, ProviderFetchFailure, ProviderPoolExhausted,
    ProviderPriorityFallback, RecordingLifecycleMessage, ScheduledTaskFailure, ServerLifecycleEvent,
    ServerLifecycleState, StreamProbeFailure, SystemInfo, UserLifecycleEvent, UserLifecycleState, WatchChanges,
    WatchDisabled, WatchUnmatched,
};
use std::sync::Arc;

/// Everything that happens in the server that someone outside the emitting
/// module might want to know about.
///
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum EventMessage {
    ServerError(String),
    /// The server finished starting, or is stopping. One variant, two kinds -
    /// see [`EventMessage::kind`].
    ServerLifecycle(ServerLifecycleEvent),
    ActiveUser(ActiveUserConnectionChange),
    ActiveProvider(Arc<str>, usize),
    ConfigChange(ConfigType),
    PlaylistUpdate(PlaylistUpdateSummary),
    PlaylistUpdateProgress(PlaylistUpdateProgressEvent),
    SystemInfoUpdate(Arc<SystemInfo>),
    LibraryScanProgress(LibraryScanProgressEvent),
    RecordingChanged,
    /// Bytes moved, nothing else. Separate from `RecordingChanged` so a
    /// session can rate limit it without also delaying a state transition.
    RecordingProgress,
    RecordingRulesChanged,
    InputMetadataUpdatesCompleted(Arc<str>),
    InputMetadataUpdatesStarted(Arc<str>),
    /// A metadata update cycle ended with tasks it could not finish.
    ///
    /// `InputMetadataUpdatesCompleted` only fires when a cycle drained *with
    /// changes*, so without this an input whose resolves always fail emits a
    /// start and then silence for as long as it stays broken.
    InputMetadataUpdatesFailed(MetadataUpdateFailure),

    // The lifecycle events below reached the notification pipeline directly,
    // never the bus, so nothing that subscribes here could see them. Each is
    // low-frequency, and each already had a registered notification id.
    /// Disk usage crossed the warn or critical threshold.
    DiskAlert(DiskAlert),
    /// A watched config file could not be reloaded. Distinct from
    /// `ServerError`: a plugin or operator can subscribe to "my config
    /// stopped loading" without taking every server error.
    ConfigReloadFailed(ConfigReloadFailure),
    /// A target's `watch` config saw its group membership change.
    PlaylistWatchChanged(WatchChanges),
    /// A target gained or lost whole groups between refreshes.
    PlaylistGroupsChanged(PlaylistGroupsChanged),
    /// A target's `watch` config is configured but not working.
    PlaylistWatchDisabled(WatchDisabled),
    /// `watch` patterns that matched no group in the refreshed playlist.
    PlaylistWatchUnmatched(WatchUnmatched),
    /// A recording started, finished or failed. One variant, three kinds -
    /// see [`EventMessage::kind`] - so a subscriber can ask for failures
    /// alone.
    RecordingLifecycle(RecordingLifecycleMessage),
    /// A provider account changed status, is about to expire, or has.
    ProviderAccount(ProviderAccountEvent),
    /// An input's playlist fetch failed.
    ProviderFetchFailed(ProviderFetchFailure),
    /// Every provider behind an input was at capacity.
    ProviderPoolExhausted(ProviderPoolExhausted),
    /// An input started being served from a different priority group.
    ProviderPriorityFallback(ProviderPriorityFallback),

    /// An API-proxy user was created, changed or removed. One variant,
    /// three kinds - see [`EventMessage::kind`] - so a subscriber can ask
    /// for deletions alone.
    UserLifecycle(UserLifecycleEvent),
    /// A user was refused a connection because their limits were full.
    ConnectionDenied(ConnectionDenied),

    /// A stream probe returned no metadata. There is no success
    /// counterpart; see [`StreamProbeFailure`].
    StreamProbeFailed(StreamProbeFailure),

    /// A scheduled task could not complete.
    ScheduledTaskFailed(ScheduledTaskFailure),

    /// A notification ran out of delivery attempts and was dropped.
    ///
    /// On the bus for plugins and the status endpoint; deliberately *not*
    /// notifiable - see [`NotificationDeadLetter`] for why routing this
    /// through the outbox that just failed would loop.
    NotificationDeadLettered(NotificationDeadLetter),

    /// An authentication decision: a sign-in, a rejected sign-in, a
    /// throttled attempt, or a permission denial. One variant, four kinds -
    /// see [`EventMessage::kind`] - so a subscriber can ask for the failures
    /// without being woken by every successful sign-in.
    AuthAudit(AuthAuditEvent),
}

impl EventMessage {
    /// Which event this is.
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        match self {
            Self::ServerError(_) => EventKind::ServerError,
            Self::ServerLifecycle(event) => match event.state {
                ServerLifecycleState::Started => EventKind::ServerStarted,
                ServerLifecycleState::ShuttingDown => EventKind::ServerShutdown,
            },
            Self::ActiveUser(_) => EventKind::ActiveUser,
            Self::ActiveProvider(_, _) => EventKind::ActiveProvider,
            Self::ConfigChange(_) => EventKind::ConfigChange,
            Self::PlaylistUpdate(_) => EventKind::PlaylistUpdate,
            Self::PlaylistUpdateProgress(_) => EventKind::PlaylistUpdateProgress,
            Self::SystemInfoUpdate(_) => EventKind::SystemInfoUpdate,
            // One payload, two kinds. The emitter sends exactly one event per
            // scan - a success or a failure - so the status is the whole
            // discriminant.
            Self::LibraryScanProgress(event) => match event.summary.status {
                LibraryScanSummaryStatus::Success => EventKind::LibraryScanProgress,
                LibraryScanSummaryStatus::Error => EventKind::LibraryScanFailed,
            },
            Self::RecordingChanged => EventKind::RecordingChanged,
            Self::RecordingProgress => EventKind::RecordingProgress,
            Self::RecordingRulesChanged => EventKind::RecordingRulesChanged,
            Self::InputMetadataUpdatesCompleted(_) => EventKind::InputMetadataUpdatesCompleted,
            Self::InputMetadataUpdatesStarted(_) => EventKind::InputMetadataUpdatesStarted,
            Self::InputMetadataUpdatesFailed(_) => EventKind::InputMetadataUpdatesFailed,
            Self::DiskAlert(_) => EventKind::DiskAlert,
            Self::ConfigReloadFailed(_) => EventKind::ConfigReloadFailed,
            Self::PlaylistWatchChanged(_) => EventKind::PlaylistWatchChanged,
            Self::PlaylistGroupsChanged(_) => EventKind::PlaylistGroupsChanged,
            Self::PlaylistWatchDisabled(_) => EventKind::PlaylistWatchDisabled,
            Self::PlaylistWatchUnmatched(_) => EventKind::PlaylistWatchUnmatched,
            // One payload, three kinds: a subscriber that only cares about
            // failures should not be woken for every completed recording.
            Self::RecordingLifecycle(msg) => match msg.event {
                MsgKind::RecordingStarted => EventKind::RecordingStarted,
                MsgKind::RecordingCompleted => EventKind::RecordingCompleted,
                // `RecordingLifecycleMessage::event` is typed as the whole
                // `MsgKind`; anything that is not a start or a completion is
                // reported as a failure rather than silently miscategorised.
                _ => EventKind::RecordingFailed,
            },
            Self::ProviderAccount(event) => match event.state {
                ProviderAccountState::StatusChanged => EventKind::ProviderAccountStatus,
                ProviderAccountState::Expiring => EventKind::ProviderAccountExpiring,
                ProviderAccountState::Expired => EventKind::ProviderAccountExpired,
            },
            Self::ProviderFetchFailed(_) => EventKind::ProviderFetchFailed,
            Self::ProviderPoolExhausted(_) => EventKind::ProviderPoolExhausted,
            Self::ProviderPriorityFallback(_) => EventKind::ProviderPriorityFallback,
            Self::UserLifecycle(event) => match event.state {
                UserLifecycleState::Created => EventKind::UserCreated,
                UserLifecycleState::Updated => EventKind::UserUpdated,
                UserLifecycleState::Deleted => EventKind::UserDeleted,
            },
            Self::ConnectionDenied(_) => EventKind::ConnectionDenied,
            Self::StreamProbeFailed(_) => EventKind::StreamProbeFailed,
            Self::NotificationDeadLettered(_) => EventKind::NotificationDeadLettered,
            Self::ScheduledTaskFailed(_) => EventKind::ScheduledTaskFailed,
            Self::AuthAudit(event) => match event.outcome {
                AuthAuditOutcome::SignInSucceeded => EventKind::AuthSignInSucceeded,
                AuthAuditOutcome::SignInFailed => EventKind::AuthSignInFailed,
                AuthAuditOutcome::SignInThrottled => EventKind::AuthSignInThrottled,
                AuthAuditOutcome::PermissionDenied => EventKind::AuthPermissionDenied,
            },
        }
    }

    /// How bad this particular occurrence is.
    ///
    /// Depends on the payload, not just the kind: a playlist update that
    /// failed is an error and one that succeeded is not.
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self {
            // The registry cannot answer this either: whether a fetch failure
            // needs a human is a property of the classification, not the id.
            Self::ProviderFetchFailed(failure) if !failure.needs_operator => Severity::Warn,
            // The only case the registry cannot answer: a partial refresh
            // and a clean one share `PLAYLIST_UPDATE_COMPLETED`, but a
            // partial one is not a clean success.
            Self::PlaylistUpdate(summary) if summary.state == PlaylistUpdateState::Partial => Severity::Warn,
            // Everything else takes the severity its registered event
            // declares, so there is no second severity table to drift.
            _ => self.notification_id().map_or(Severity::Info, registry::default_severity),
        }
    }

    /// The notification this event becomes, if it is notifiable at all.
    ///
    /// On `EventMessage` rather than `EventKind` because two of them depend
    /// on the payload: a playlist update that failed is a different
    /// notification from one that succeeded, not merely a more severe one.
    ///
    /// The bridge used to hold this table itself, so the event taxonomy and
    /// the notification taxonomy drifted independently. Now the event says
    /// what it is and the bridge only decides how to word it.
    ///
    /// `None` is the honest answer for the high-frequency kinds: they fire
    /// many times per operation and their terminal counterparts carry the
    /// news.
    #[must_use]
    pub const fn notification_id(&self) -> Option<EventId> {
        Some(match self {
            Self::ServerError(_) => registry::SYSTEM_ERROR,
            Self::ServerLifecycle(event) => match event.state {
                ServerLifecycleState::Started => registry::SYSTEM_STARTED,
                ServerLifecycleState::ShuttingDown => registry::SYSTEM_SHUTDOWN,
            },
            Self::PlaylistUpdate(summary) => match summary.state {
                PlaylistUpdateState::Success | PlaylistUpdateState::Partial => registry::PLAYLIST_UPDATE_COMPLETED,
                PlaylistUpdateState::Failure => registry::PLAYLIST_UPDATE_FAILED,
            },
            Self::ConfigChange(_) => registry::CONFIG_CHANGED,
            // A failed scan used to take `LIBRARY_SCAN_COMPLETED` like any
            // other, so operators were told "A local library scan finished"
            // at info severity when it had not.
            Self::LibraryScanProgress(event) => match event.summary.status {
                LibraryScanSummaryStatus::Success => registry::LIBRARY_SCAN_COMPLETED,
                LibraryScanSummaryStatus::Error => registry::LIBRARY_SCAN_FAILED,
            },
            Self::InputMetadataUpdatesStarted(_) => registry::METADATA_UPDATE_STARTED,
            Self::InputMetadataUpdatesCompleted(_) => registry::METADATA_UPDATE_COMPLETED,
            Self::InputMetadataUpdatesFailed(_) => registry::METADATA_UPDATE_FAILED,
            Self::DiskAlert(_) => registry::SYSTEM_DISK_ALERT,
            Self::ConfigReloadFailed(_) => registry::CONFIG_RELOAD_FAILED,
            Self::PlaylistWatchChanged(_) => registry::PLAYLIST_WATCH_CHANGED,
            Self::PlaylistGroupsChanged(_) => registry::PLAYLIST_GROUPS_CHANGED,
            Self::PlaylistWatchDisabled(_) => registry::PLAYLIST_WATCH_DISABLED,
            Self::PlaylistWatchUnmatched(_) => registry::PLAYLIST_WATCH_UNMATCHED,
            Self::RecordingLifecycle(msg) => match msg.event {
                MsgKind::RecordingStarted => registry::RECORDING_STARTED,
                MsgKind::RecordingCompleted => registry::RECORDING_COMPLETED,
                _ => registry::RECORDING_FAILED,
            },
            Self::ProviderAccount(event) => match event.state {
                ProviderAccountState::StatusChanged => registry::PROVIDER_ACCOUNT_STATUS,
                ProviderAccountState::Expiring => registry::PROVIDER_ACCOUNT_EXPIRING,
                ProviderAccountState::Expired => registry::PROVIDER_ACCOUNT_EXPIRED,
            },
            Self::ProviderFetchFailed(_) => registry::PROVIDER_FETCH_FAILED,
            Self::ProviderPoolExhausted(_) => registry::PROVIDER_POOL_EXHAUSTED,
            Self::ProviderPriorityFallback(_) => registry::PROVIDER_PRIORITY_FALLBACK,
            Self::UserLifecycle(event) => match event.state {
                UserLifecycleState::Created => registry::USER_CREATED,
                UserLifecycleState::Updated => registry::USER_UPDATED,
                UserLifecycleState::Deleted => registry::USER_DELETED,
            },
            Self::ConnectionDenied(_) => registry::USER_CONNECTION_DENIED,
            Self::StreamProbeFailed(_) => registry::STREAM_PROBE_FAILED,
            Self::NotificationDeadLettered(_) => registry::NOTIFICATION_DEAD_LETTERED,
            Self::ScheduledTaskFailed(_) => registry::SCHEDULED_TASK_FAILED,
            Self::AuthAudit(event) => match event.outcome {
                AuthAuditOutcome::SignInSucceeded => registry::AUTH_SIGN_IN_SUCCEEDED,
                AuthAuditOutcome::SignInFailed => registry::AUTH_SIGN_IN_FAILED,
                AuthAuditOutcome::SignInThrottled => registry::AUTH_SIGN_IN_THROTTLED,
                AuthAuditOutcome::PermissionDenied => registry::AUTH_PERMISSION_DENIED,
            },
            Self::ActiveUser(_) => registry::USER_CONNECTION_CHANGED,
            Self::ActiveProvider(_, _) => registry::PROVIDER_CONNECTIONS_CHANGED,
            Self::RecordingChanged => registry::RECORDING_QUEUE_CHANGED,
            Self::RecordingRulesChanged => registry::RECORDING_RULES_CHANGED,
            Self::PlaylistUpdateProgress(_) | Self::SystemInfoUpdate(_) | Self::RecordingProgress => return None,
        })
    }

    /// The event as JSON, for consumers that do not speak Rust types.
    ///
    /// Plugins are handed a wire name and a JSON payload rather than the
    /// enum, so this is what a plugin host serialises. Defining it here means
    /// a new variant arrives with its payload shape already decided, instead
    /// of the host growing a second `match` over the whole taxonomy.
    ///
    /// Never fails: a payload that will not serialise degrades to `null`
    /// rather than dropping the event, because a plugin that learns a
    /// refresh finished but not its statistics is still better served than
    /// one that hears nothing.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        fn encode<T: serde::Serialize>(value: &T) -> serde_json::Value {
            serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
        }
        match self {
            Self::ServerError(error) => serde_json::json!({ "error": error }),
            Self::ServerLifecycle(event) => encode(event),
            Self::ActiveUser(change) => encode(change),
            Self::ActiveProvider(name, connections) => {
                serde_json::json!({ "provider": name.as_ref(), "connections": connections })
            }
            // `ConfigType` is not `Serialize`; its display form is the
            // stable name operators already see in the Web UI.
            Self::ConfigChange(config_type) => serde_json::json!({ "config_type": config_type.to_string() }),
            Self::PlaylistUpdate(summary) => encode(summary),
            Self::PlaylistUpdateProgress(progress) => encode(progress),
            Self::SystemInfoUpdate(info) => encode(info.as_ref()),
            Self::LibraryScanProgress(progress) => encode(progress),
            Self::RecordingChanged | Self::RecordingProgress | Self::RecordingRulesChanged => serde_json::Value::Null,
            Self::InputMetadataUpdatesStarted(input) | Self::InputMetadataUpdatesCompleted(input) => {
                serde_json::json!({ "input": input.as_ref() })
            }
            Self::InputMetadataUpdatesFailed(failure) => encode(failure),
            Self::DiskAlert(alert) => encode(alert),
            Self::ConfigReloadFailed(failure) => encode(failure),
            Self::PlaylistWatchChanged(changes) => encode(changes),
            Self::PlaylistGroupsChanged(changes) => encode(changes),
            Self::PlaylistWatchDisabled(disabled) => encode(disabled),
            Self::PlaylistWatchUnmatched(unmatched) => encode(unmatched),
            Self::RecordingLifecycle(msg) => encode(msg),
            Self::ProviderAccount(event) => encode(event),
            Self::ProviderFetchFailed(failure) => encode(failure),
            Self::ProviderPoolExhausted(exhausted) => encode(exhausted),
            Self::ProviderPriorityFallback(fallback) => encode(fallback),
            Self::UserLifecycle(event) => encode(event),
            Self::ConnectionDenied(denied) => encode(denied),
            Self::StreamProbeFailed(failure) => encode(failure),
            Self::NotificationDeadLettered(dead_letter) => encode(dead_letter),
            Self::ScheduledTaskFailed(failure) => encode(failure),
            Self::AuthAudit(event) => encode(event),
        }
    }

    /// See [`EventKind::required_permission`].
    #[must_use]
    pub const fn required_permission(&self) -> Permission { self.kind().required_permission() }

    /// See [`EventKind::is_high_frequency`].
    #[must_use]
    pub const fn is_high_frequency(&self) -> bool { self.kind().is_high_frequency() }
}

/// What one playlist refresh did.
///
/// `PlaylistUpdate` used to carry the state alone, so "the refresh finished"
/// reached the bus but what it actually did did not - the run summary went
/// straight to the notification layer as a second, separate message with the
/// same registered id. Subscribers saw an outcome with no detail, operators
/// received two notifications per refresh, and a plugin asking for
/// `playlist.update` got a bare enum.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlaylistUpdateSummary {
    /// Stable identity of the processing run. Missing only on legacy/test-only events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<PlaylistUpdateRunId>,
    /// Actual serial execution order. Missing only on legacy/test-only events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_order: Option<PlaylistUpdateRunOrder>,
    pub state: PlaylistUpdateState,
    /// Per-source statistics. Empty when the run produced none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stats: Vec<SourceStats>,
    /// The aggregated error text, when the run reported errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PlaylistUpdateSummary {
    /// A summary carrying only an outcome - the timeout and panic paths,
    /// which have no statistics to report.
    #[must_use]
    pub fn state_only(state: PlaylistUpdateState) -> Self {
        Self { run_id: None, execution_order: None, state, stats: Vec::new(), error: None }
    }

    #[must_use]
    pub fn for_run(
        run_id: PlaylistUpdateRunId,
        execution_order: PlaylistUpdateRunOrder,
        state: PlaylistUpdateState,
    ) -> Self {
        Self { run_id: Some(run_id), execution_order: Some(execution_order), state, stats: Vec::new(), error: None }
    }
}
