//! The in-process event taxonomy.
//!
//! `EventMessage` used to live in `tuliprox-session`, which meant that
//! `metadata`, `dvr` and `processing` all had to depend on the *streaming
//! session runtime* just to name an event. A metadata refresh has nothing
//! to do with provider allocation, so the type lives here instead: `shared`
//! is the one crate every emitter already sees.
//!
//! The bus implementation (`EventManager`) stays in `session`, where the
//! stream-meter registry it feeds also lives.

use crate::model::{
    auth_audit::{AuthAuditEvent, AuthAuditOutcome},
    notification::{registry, EventId, Severity},
    stats::SourceStats,
    ActiveUserConnectionChange, ConfigReloadFailure, ConfigType, DiskAlert, DownloadsDelta, DownloadsResponse,
    LibraryScanProgressEvent, LibraryScanSummaryStatus, MetadataUpdateFailure, MsgKind, Permission,
    PlaylistUpdateProgressEvent, PlaylistUpdateState, ProviderAccountEvent, ProviderAccountState,
    RecordingLifecycleMessage, ServerLifecycleEvent, ServerLifecycleState, StreamProbeFailure, SystemInfo,
    UserLifecycleEvent, UserLifecycleState, WatchChanges,
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
    DownloadsUpdate(Arc<DownloadsResponse>),
    DownloadsDeltaUpdate(DownloadsDelta),
    RecordingChanged,
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
    /// A recording started, finished or failed. One variant, three kinds -
    /// see [`EventMessage::kind`] - so a subscriber can ask for failures
    /// alone.
    RecordingLifecycle(RecordingLifecycleMessage),
    /// A provider account changed status, is about to expire, or has.
    ProviderAccount(ProviderAccountEvent),

    /// An API-proxy user was created, changed or removed. One variant,
    /// three kinds - see [`EventMessage::kind`] - so a subscriber can ask
    /// for deletions alone.
    UserLifecycle(UserLifecycleEvent),
    /// A stream probe returned no metadata. There is no success
    /// counterpart; see [`StreamProbeFailure`].
    StreamProbeFailed(StreamProbeFailure),

    /// An authentication decision: a sign-in, a rejected sign-in, a
    /// throttled attempt, or a permission denial. One variant, four kinds -
    /// see [`EventMessage::kind`] - so a subscriber can ask for the failures
    /// without being woken by every successful sign-in.
    AuthAudit(AuthAuditEvent),
}

/// Somewhere an [`EventMessage`] can be published.
///
/// A bound, not a trait object: emitters are generic over their sink and
/// monomorphise against the one they were built with. Three things fall out
/// of that:
///
/// * the pipeline no longer threads `Option<Arc<EventManager>>` around and
///   branches on it at every emit site - [`NoopSink`] is the absent case,
///   and its `emit` is an empty function the optimiser deletes outright;
/// * tests can assert on what was emitted without standing up a broadcast
///   channel and racing a subscriber;
/// * `dvr` and the notification path can name a sink without depending on
///   the streaming-session runtime that implements it.
///
/// Implementations must not block. The bus is reached from the streaming
/// data path, and an emitter that can stall is an emitter that will.
pub trait EventSink: Send + Sync {
    /// Publish. Best-effort by contract: no subscribers, a full buffer or a
    /// closed channel are all normal and none of them are the emitter's
    /// problem.
    fn emit(&self, event: EventMessage);
}

/// The sink that drops everything.
///
/// Replaces the `Option` at call sites that only ever had a `None` case
/// because tests and one-shot CLI runs have no bus. Monomorphised against
/// this, every emit site compiles to nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: EventMessage) {}
}

impl<T: EventSink + ?Sized> EventSink for Arc<T> {
    fn emit(&self, event: EventMessage) { (**self).emit(event); }
}

impl<T: EventSink> EventSink for &T {
    fn emit(&self, event: EventMessage) { (**self).emit(event); }
}

/// Which event this is, without its payload.
///
/// Subscribers used to discriminate by exhaustively matching `EventMessage`,
/// once per subscriber: the websocket mapped variants to permissions, the
/// notification bridge mapped them to notification ids with four arms
/// returning `None`, and the wire layer mapped them to `ProtocolMessage`.
/// Adding a variant compiled cleanly while silently reaching none of them.
///
/// Everything that is a property *of the event* rather than of one consumer
/// hangs off this type instead, so a new variant has one place to declare
/// what it is and every consumer picks it up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventKind {
    ServerError,
    ServerStarted,
    ServerShutdown,
    ActiveUser,
    ActiveProvider,
    ConfigChange,
    PlaylistUpdate,
    PlaylistUpdateProgress,
    SystemInfoUpdate,
    LibraryScanProgress,
    /// A scan that ended in an error. Separate from the progress kind so a
    /// subscriber can take scan failures without the tick firehose.
    LibraryScanFailed,
    DownloadsUpdate,
    DownloadsDeltaUpdate,
    RecordingChanged,
    RecordingRulesChanged,
    InputMetadataUpdatesCompleted,
    InputMetadataUpdatesStarted,
    InputMetadataUpdatesFailed,
    DiskAlert,
    ConfigReloadFailed,
    PlaylistWatchChanged,
    RecordingStarted,
    RecordingCompleted,
    RecordingFailed,
    ProviderAccountStatus,
    ProviderAccountExpiring,
    ProviderAccountExpired,
    UserCreated,
    UserUpdated,
    UserDeleted,
    StreamProbeFailed,
    AuthSignInSucceeded,
    AuthSignInFailed,
    AuthSignInThrottled,
    AuthPermissionDenied,
}

impl EventKind {
    /// Every kind, in declaration order.
    ///
    /// The mask type below indexes into this, so the order is load-bearing:
    /// it is the bit order, not just a listing.
    pub const ALL: [Self; 35] = [
        Self::ServerError,
        Self::ServerStarted,
        Self::ServerShutdown,
        Self::ActiveUser,
        Self::ActiveProvider,
        Self::ConfigChange,
        Self::PlaylistUpdate,
        Self::PlaylistUpdateProgress,
        Self::SystemInfoUpdate,
        Self::LibraryScanProgress,
        Self::LibraryScanFailed,
        Self::DownloadsUpdate,
        Self::DownloadsDeltaUpdate,
        Self::RecordingChanged,
        Self::RecordingRulesChanged,
        Self::InputMetadataUpdatesCompleted,
        Self::InputMetadataUpdatesStarted,
        Self::InputMetadataUpdatesFailed,
        Self::DiskAlert,
        Self::ConfigReloadFailed,
        Self::PlaylistWatchChanged,
        Self::RecordingStarted,
        Self::RecordingCompleted,
        Self::RecordingFailed,
        Self::ProviderAccountStatus,
        Self::ProviderAccountExpiring,
        Self::ProviderAccountExpired,
        Self::UserCreated,
        Self::UserUpdated,
        Self::UserDeleted,
        Self::StreamProbeFailed,
        Self::AuthSignInSucceeded,
        Self::AuthSignInFailed,
        Self::AuthSignInThrottled,
        Self::AuthPermissionDenied,
    ];

    /// This kind's bit position.
    ///
    /// `u64`, not `u32`: the taxonomy passed 23 kinds and the headroom above
    /// it is where a mask migration would have to happen *after* operators
    /// had written subscriptions into config. Widening now is free.
    #[must_use]
    pub const fn bit(self) -> u64 { 1 << (self as u32) }

    /// The permission a websocket session must hold to receive this kind.
    ///
    /// Lives here rather than in the websocket handler because it is a fact
    /// about the event: "who may see a download delta" does not change with
    /// the transport carrying it.
    #[must_use]
    pub const fn required_permission(self) -> Permission {
        match self {
            Self::DownloadsUpdate | Self::DownloadsDeltaUpdate => Permission::DownloadRead,
            Self::RecordingChanged
            | Self::RecordingRulesChanged
            | Self::RecordingStarted
            | Self::RecordingCompleted
            | Self::RecordingFailed => Permission::RecordingRead,
            Self::PlaylistUpdate | Self::PlaylistUpdateProgress | Self::PlaylistWatchChanged => {
                Permission::PlaylistWrite
            }
            Self::LibraryScanProgress | Self::LibraryScanFailed => Permission::LibraryWrite,
            // Who may hear that an account was created is the same question
            // as who may list accounts.
            Self::UserCreated | Self::UserUpdated | Self::UserDeleted => Permission::UserRead,
            // Who signed in, who failed to, and who was refused: the same
            // question as who may read the user list, and strictly narrower
            // than the system-wide read the other operational events take.
            Self::AuthSignInSucceeded
            | Self::AuthSignInFailed
            | Self::AuthSignInThrottled
            | Self::AuthPermissionDenied => Permission::UserRead,
            Self::ServerError
            | Self::ServerStarted
            | Self::ServerShutdown
            | Self::ActiveUser
            | Self::ActiveProvider
            | Self::ConfigChange
            | Self::SystemInfoUpdate
            | Self::InputMetadataUpdatesCompleted
            | Self::InputMetadataUpdatesStarted
            | Self::InputMetadataUpdatesFailed
            | Self::DiskAlert
            | Self::ConfigReloadFailed
            | Self::ProviderAccountStatus
            | Self::ProviderAccountExpiring
            | Self::ProviderAccountExpired
            // Sits with the metadata-update events it is produced by.
            | Self::StreamProbeFailed => Permission::SystemRead,
        }
    }

    /// Does this kind fire many times per operation?
    ///
    /// True for progress ticks, incremental deltas and the periodic
    /// system-info sample. A bus that coalesces needs to know which messages
    /// are safe to supersede, and a subscriber sizing its buffer needs to
    /// know which ones will fill it.
    ///
    /// This is a statement about rate, not about whether anyone wants the
    /// event: the notification bridge decides notifiability separately,
    /// because that also depends on whether a terminal counterpart exists.
    #[must_use]
    pub const fn is_high_frequency(self) -> bool {
        matches!(
            self,
            Self::PlaylistUpdateProgress
                | Self::LibraryScanProgress
                | Self::DownloadsUpdate
                | Self::DownloadsDeltaUpdate
                | Self::SystemInfoUpdate
        )
    }

    /// Does this kind describe current state rather than an occurrence?
    ///
    /// A latched kind's newest message is the whole truth - the last
    /// `SystemInfo` sample *is* the system info - so it is worth retaining
    /// for a subscriber that connects later. An occurrence is not: replaying
    /// "a playlist update finished" to a session that was not there when it
    /// happened would be a lie.
    ///
    /// This is what a cold websocket connect should be handed instead of
    /// waiting up to three seconds for the next sample.
    #[must_use]
    pub const fn is_latched(self) -> bool {
        matches!(self, Self::SystemInfoUpdate | Self::DownloadsUpdate | Self::ActiveProvider)
    }

    /// Is this kind a payload-free nudge that can be coalesced?
    ///
    /// True only where N occurrences and one are indistinguishable to every
    /// consumer: the event carries no data and everyone who receives it
    /// responds by re-reading current state. Deleting a recording emits
    /// `RecordingChanged` and `RecordingRulesChanged` back to back, and a
    /// bulk operation emits one per item; each makes the Web UI re-fetch the
    /// same snapshot.
    ///
    /// Never true for an event carrying a payload, however repetitive - a
    /// dropped progress tick loses the message it carried.
    #[must_use]
    pub const fn is_coalescable(self) -> bool { matches!(self, Self::RecordingChanged | Self::RecordingRulesChanged) }

    /// Stable wire name.
    ///
    /// Plugins are compiled against these strings and operators write them
    /// into subscription config, so - like a notification channel id - a
    /// released name must not change.
    #[must_use]
    pub const fn as_wire_name(self) -> &'static str {
        match self {
            Self::ServerError => "server.error",
            Self::ServerStarted => "system.started",
            Self::ServerShutdown => "system.shutdown",
            Self::ActiveUser => "user.connection.changed",
            Self::ActiveProvider => "provider.connection.changed",
            Self::ConfigChange => "config.changed",
            Self::PlaylistUpdate => "playlist.update",
            Self::PlaylistUpdateProgress => "playlist.update.progress",
            Self::SystemInfoUpdate => "system.info",
            Self::LibraryScanProgress => "library.scan.progress",
            Self::LibraryScanFailed => "library.scan.failed",
            Self::DownloadsUpdate => "downloads.update",
            Self::DownloadsDeltaUpdate => "downloads.delta",
            Self::RecordingChanged => "recording.changed",
            Self::RecordingRulesChanged => "recording.rules.changed",
            Self::InputMetadataUpdatesCompleted => "metadata.update.completed",
            Self::InputMetadataUpdatesStarted => "metadata.update.started",
            Self::InputMetadataUpdatesFailed => "metadata.update.failed",
            Self::DiskAlert => "system.disk.alert",
            Self::ConfigReloadFailed => "config.reload.failed",
            Self::PlaylistWatchChanged => "playlist.watch.changed",
            Self::RecordingStarted => "recording.started",
            Self::RecordingCompleted => "recording.completed",
            Self::RecordingFailed => "recording.failed",
            Self::ProviderAccountStatus => "provider.account.status",
            Self::ProviderAccountExpiring => "provider.account.expiring",
            Self::ProviderAccountExpired => "provider.account.expired",
            Self::UserCreated => "user.created",
            Self::UserUpdated => "user.updated",
            Self::UserDeleted => "user.deleted",
            Self::StreamProbeFailed => "stream.probe.failed",
            Self::AuthSignInSucceeded => "auth.sign_in.succeeded",
            Self::AuthSignInFailed => "auth.sign_in.failed",
            Self::AuthSignInThrottled => "auth.sign_in.throttled",
            Self::AuthPermissionDenied => "auth.permission.denied",
        }
    }

    /// Parse a wire name back. Unknown names are `None` rather than a
    /// fallback variant, so a typo in a subscription is visible.
    #[must_use]
    pub fn from_wire_name(name: &str) -> Option<Self> { Self::ALL.into_iter().find(|kind| kind.as_wire_name() == name) }
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
            Self::DownloadsUpdate(_) => EventKind::DownloadsUpdate,
            Self::DownloadsDeltaUpdate(_) => EventKind::DownloadsDeltaUpdate,
            Self::RecordingChanged => EventKind::RecordingChanged,
            Self::RecordingRulesChanged => EventKind::RecordingRulesChanged,
            Self::InputMetadataUpdatesCompleted(_) => EventKind::InputMetadataUpdatesCompleted,
            Self::InputMetadataUpdatesStarted(_) => EventKind::InputMetadataUpdatesStarted,
            Self::InputMetadataUpdatesFailed(_) => EventKind::InputMetadataUpdatesFailed,
            Self::DiskAlert(_) => EventKind::DiskAlert,
            Self::ConfigReloadFailed(_) => EventKind::ConfigReloadFailed,
            Self::PlaylistWatchChanged(_) => EventKind::PlaylistWatchChanged,
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
            Self::UserLifecycle(event) => match event.state {
                UserLifecycleState::Created => EventKind::UserCreated,
                UserLifecycleState::Updated => EventKind::UserUpdated,
                UserLifecycleState::Deleted => EventKind::UserDeleted,
            },
            Self::StreamProbeFailed(_) => EventKind::StreamProbeFailed,
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
            Self::UserLifecycle(event) => match event.state {
                UserLifecycleState::Created => registry::USER_CREATED,
                UserLifecycleState::Updated => registry::USER_UPDATED,
                UserLifecycleState::Deleted => registry::USER_DELETED,
            },
            Self::StreamProbeFailed(_) => registry::STREAM_PROBE_FAILED,
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
            Self::PlaylistUpdateProgress(_)
            | Self::SystemInfoUpdate(_)
            | Self::DownloadsUpdate(_)
            | Self::DownloadsDeltaUpdate(_) => return None,
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
            Self::DownloadsUpdate(downloads) => encode(downloads.as_ref()),
            Self::DownloadsDeltaUpdate(delta) => encode(delta),
            Self::RecordingChanged | Self::RecordingRulesChanged => serde_json::Value::Null,
            Self::InputMetadataUpdatesStarted(input) | Self::InputMetadataUpdatesCompleted(input) => {
                serde_json::json!({ "input": input.as_ref() })
            }
            Self::InputMetadataUpdatesFailed(failure) => encode(failure),
            Self::DiskAlert(alert) => encode(alert),
            Self::ConfigReloadFailed(failure) => encode(failure),
            Self::PlaylistWatchChanged(changes) => encode(changes),
            Self::RecordingLifecycle(msg) => encode(msg),
            Self::ProviderAccount(event) => encode(event),
            Self::UserLifecycle(event) => encode(event),
            Self::StreamProbeFailed(failure) => encode(failure),
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
    pub fn state_only(state: PlaylistUpdateState) -> Self { Self { state, stats: Vec::new(), error: None } }
}

/// A set of [`EventKind`]s, as one word.
///
/// `get_event_channel` handed every subscriber the firehose, and each one
/// filtered afterwards - after the broadcast channel had already cloned the
/// message for it. A subscriber that wants two kinds should not pay for the
/// other twelve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EventKindMask(u64);

impl EventKindMask {
    /// Nothing.
    pub const NONE: Self = Self(0);

    /// Everything, including kinds added after this mask was written.
    pub const ALL: Self = Self(u64::MAX);

    /// An empty mask to build on.
    #[must_use]
    pub const fn new() -> Self { Self::NONE }

    /// This mask plus `kind`.
    #[must_use]
    pub const fn with(self, kind: EventKind) -> Self { Self(self.0 | kind.bit()) }

    /// Is `kind` in this mask?
    #[must_use]
    pub const fn contains(self, kind: EventKind) -> bool { self.0 & kind.bit() != 0 }

    /// Is this mask empty? A subscriber with nothing selected should not be
    /// spawned at all.
    #[must_use]
    pub const fn is_empty(self) -> bool { self.0 == 0 }

    /// The union of two masks - how a plugin host builds one mask covering
    /// every loaded plugin's subscriptions.
    #[must_use]
    pub const fn union(self, other: Self) -> Self { Self(self.0 | other.0) }
}

impl EventKindMask {
    /// Build a mask from wire names - a plugin manifest's `events.*`
    /// subscription list, or an operator's config.
    ///
    /// Returns the mask alongside the names that matched nothing, so a typo
    /// in a subscription can be reported rather than silently subscribing to
    /// less than was asked for.
    #[must_use]
    pub fn from_wire_names<'a, I>(names: I) -> (Self, Vec<&'a str>)
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut mask = Self::NONE;
        let mut unknown = Vec::new();
        for name in names {
            match EventKind::from_wire_name(name) {
                Some(kind) => mask = mask.with(kind),
                None => unknown.push(name),
            }
        }
        (mask, unknown)
    }

    /// The kinds in this mask.
    #[must_use]
    pub fn kinds(self) -> Vec<EventKind> { EventKind::ALL.into_iter().filter(|kind| self.contains(*kind)).collect() }
}

impl FromIterator<EventKind> for EventKindMask {
    fn from_iter<I: IntoIterator<Item = EventKind>>(iter: I) -> Self { iter.into_iter().fold(Self::NONE, Self::with) }
}

#[cfg(test)]
mod tests {
    use super::{EventKind, EventKindMask};

    #[test]
    fn every_kind_has_a_distinct_bit() {
        let mut seen = 0u64;
        for kind in EventKind::ALL {
            assert_eq!(seen & kind.bit(), 0, "{kind:?} shares a bit with an earlier kind");
            seen |= kind.bit();
        }
    }

    /// `EventKindMask` is a `u64`, so the taxonomy cannot outgrow 64 kinds
    /// without the mask type changing with it. It was a `u32` at 23 kinds;
    /// the widening happened while no operator had a subscription list to
    /// migrate, which is the only cheap time to do it.
    #[test]
    fn the_taxonomy_still_fits_in_the_mask() {
        assert!(EventKind::ALL.len() <= 64, "EventKindMask needs a wider integer");
    }

    #[test]
    fn wire_names_are_unique_and_round_trip() {
        let mut names: Vec<&str> = EventKind::ALL.iter().map(|kind| kind.as_wire_name()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two kinds share a wire name");

        for kind in EventKind::ALL {
            assert_eq!(EventKind::from_wire_name(kind.as_wire_name()), Some(kind));
        }
    }

    #[test]
    fn an_unknown_subscription_name_is_reported_not_ignored() {
        let (mask, unknown) = EventKindMask::from_wire_names(["playlist.update", "playlist.updat", "config.changed"]);

        assert!(mask.contains(EventKind::PlaylistUpdate));
        assert!(mask.contains(EventKind::ConfigChange));
        assert_eq!(unknown, vec!["playlist.updat"], "a typo must surface, not silently narrow the subscription");
    }

    #[test]
    fn a_mask_round_trips_through_its_kinds() {
        let mask = EventKindMask::from_iter([EventKind::ServerError, EventKind::RecordingChanged]);
        assert_eq!(mask.kinds(), vec![EventKind::ServerError, EventKind::RecordingChanged]);
        assert!(!mask.contains(EventKind::SystemInfoUpdate));
        assert!(!mask.is_empty());
        assert!(EventKindMask::NONE.is_empty());
    }

    #[test]
    fn all_matches_every_kind() {
        for kind in EventKind::ALL {
            assert!(EventKindMask::ALL.contains(kind));
        }
    }
}
