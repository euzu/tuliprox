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
    notification::Severity, ActiveUserConnectionChange, ConfigType, DownloadsDelta, DownloadsResponse,
    LibraryScanProgressEvent, Permission, PlaylistUpdateProgressEvent, PlaylistUpdateState, SystemInfo,
};
use std::sync::Arc;

/// Everything that happens in the server that someone outside the emitting
/// module might want to know about.
///
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum EventMessage {
    ServerError(String),
    ActiveUser(ActiveUserConnectionChange),
    ActiveProvider(Arc<str>, usize),
    ConfigChange(ConfigType),
    PlaylistUpdate(PlaylistUpdateState),
    PlaylistUpdateProgress(PlaylistUpdateProgressEvent),
    SystemInfoUpdate(SystemInfo),
    LibraryScanProgress(LibraryScanProgressEvent),
    DownloadsUpdate(DownloadsResponse),
    DownloadsDeltaUpdate(DownloadsDelta),
    RecordingChanged,
    RecordingRulesChanged,
    InputMetadataUpdatesCompleted(Arc<str>),
    InputMetadataUpdatesStarted(Arc<str>),
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
    ActiveUser,
    ActiveProvider,
    ConfigChange,
    PlaylistUpdate,
    PlaylistUpdateProgress,
    SystemInfoUpdate,
    LibraryScanProgress,
    DownloadsUpdate,
    DownloadsDeltaUpdate,
    RecordingChanged,
    RecordingRulesChanged,
    InputMetadataUpdatesCompleted,
    InputMetadataUpdatesStarted,
}

impl EventKind {
    /// Every kind, in declaration order.
    ///
    /// The mask type below indexes into this, so the order is load-bearing:
    /// it is the bit order, not just a listing.
    pub const ALL: [Self; 14] = [
        Self::ServerError,
        Self::ActiveUser,
        Self::ActiveProvider,
        Self::ConfigChange,
        Self::PlaylistUpdate,
        Self::PlaylistUpdateProgress,
        Self::SystemInfoUpdate,
        Self::LibraryScanProgress,
        Self::DownloadsUpdate,
        Self::DownloadsDeltaUpdate,
        Self::RecordingChanged,
        Self::RecordingRulesChanged,
        Self::InputMetadataUpdatesCompleted,
        Self::InputMetadataUpdatesStarted,
    ];

    /// This kind's bit position.
    #[must_use]
    pub const fn bit(self) -> u32 { 1 << (self as u32) }

    /// The permission a websocket session must hold to receive this kind.
    ///
    /// Lives here rather than in the websocket handler because it is a fact
    /// about the event: "who may see a download delta" does not change with
    /// the transport carrying it.
    #[must_use]
    pub const fn required_permission(self) -> Permission {
        match self {
            Self::DownloadsUpdate | Self::DownloadsDeltaUpdate => Permission::DownloadRead,
            Self::RecordingChanged | Self::RecordingRulesChanged => Permission::RecordingRead,
            Self::PlaylistUpdate | Self::PlaylistUpdateProgress => Permission::PlaylistWrite,
            Self::LibraryScanProgress => Permission::LibraryWrite,
            Self::ServerError
            | Self::ActiveUser
            | Self::ActiveProvider
            | Self::ConfigChange
            | Self::SystemInfoUpdate
            | Self::InputMetadataUpdatesCompleted
            | Self::InputMetadataUpdatesStarted => Permission::SystemRead,
        }
    }

    /// Does this kind fire many times per operation?
    ///
    /// High-frequency kinds are progress ticks and incremental deltas: a
    /// consumer that pushes to a phone wants their terminal counterparts and
    /// not these, and a bus that coalesces wants to know which messages are
    /// safe to supersede.
    #[must_use]
    pub const fn is_high_frequency(self) -> bool {
        matches!(
            self,
            Self::PlaylistUpdateProgress
                | Self::LibraryScanProgress
                | Self::DownloadsUpdate
                | Self::DownloadsDeltaUpdate
                | Self::SystemInfoUpdate
                | Self::ActiveUser
                | Self::ActiveProvider
        )
    }

    /// Stable wire name.
    ///
    /// Plugins are compiled against these strings and operators write them
    /// into subscription config, so - like a notification channel id - a
    /// released name must not change.
    #[must_use]
    pub const fn as_wire_name(self) -> &'static str {
        match self {
            Self::ServerError => "server.error",
            Self::ActiveUser => "user.connection.changed",
            Self::ActiveProvider => "provider.connection.changed",
            Self::ConfigChange => "config.changed",
            Self::PlaylistUpdate => "playlist.update",
            Self::PlaylistUpdateProgress => "playlist.update.progress",
            Self::SystemInfoUpdate => "system.info",
            Self::LibraryScanProgress => "library.scan.progress",
            Self::DownloadsUpdate => "downloads.update",
            Self::DownloadsDeltaUpdate => "downloads.delta",
            Self::RecordingChanged => "recording.changed",
            Self::RecordingRulesChanged => "recording.rules.changed",
            Self::InputMetadataUpdatesCompleted => "metadata.update.completed",
            Self::InputMetadataUpdatesStarted => "metadata.update.started",
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
            Self::ActiveUser(_) => EventKind::ActiveUser,
            Self::ActiveProvider(_, _) => EventKind::ActiveProvider,
            Self::ConfigChange(_) => EventKind::ConfigChange,
            Self::PlaylistUpdate(_) => EventKind::PlaylistUpdate,
            Self::PlaylistUpdateProgress(_) => EventKind::PlaylistUpdateProgress,
            Self::SystemInfoUpdate(_) => EventKind::SystemInfoUpdate,
            Self::LibraryScanProgress(_) => EventKind::LibraryScanProgress,
            Self::DownloadsUpdate(_) => EventKind::DownloadsUpdate,
            Self::DownloadsDeltaUpdate(_) => EventKind::DownloadsDeltaUpdate,
            Self::RecordingChanged => EventKind::RecordingChanged,
            Self::RecordingRulesChanged => EventKind::RecordingRulesChanged,
            Self::InputMetadataUpdatesCompleted(_) => EventKind::InputMetadataUpdatesCompleted,
            Self::InputMetadataUpdatesStarted(_) => EventKind::InputMetadataUpdatesStarted,
        }
    }

    /// How bad this particular occurrence is.
    ///
    /// Depends on the payload, not just the kind: a playlist update that
    /// failed is an error and one that succeeded is not.
    #[must_use]
    pub const fn severity(&self) -> Severity {
        match self {
            Self::ServerError(_) => Severity::Error,
            Self::PlaylistUpdate(PlaylistUpdateState::Failure) => Severity::Error,
            Self::PlaylistUpdate(PlaylistUpdateState::Partial) => Severity::Warn,
            _ => Severity::Info,
        }
    }

    /// See [`EventKind::required_permission`].
    #[must_use]
    pub const fn required_permission(&self) -> Permission { self.kind().required_permission() }

    /// See [`EventKind::is_high_frequency`].
    #[must_use]
    pub const fn is_high_frequency(&self) -> bool { self.kind().is_high_frequency() }
}
