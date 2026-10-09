use super::EventKindMask;
use crate::model::Permission;

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
    RecordingChanged,
    RecordingRulesChanged,
    InputMetadataUpdatesCompleted,
    InputMetadataUpdatesStarted,
    InputMetadataUpdatesFailed,
    DiskAlert,
    ConfigReloadFailed,
    PlaylistWatchChanged,
    PlaylistGroupsChanged,
    PlaylistWatchDisabled,
    PlaylistWatchUnmatched,
    RecordingStarted,
    RecordingCompleted,
    RecordingFailed,
    ProviderAccountStatus,
    ProviderAccountExpiring,
    ProviderAccountExpired,
    ProviderFetchFailed,
    ProviderPoolExhausted,
    ProviderPriorityFallback,
    UserCreated,
    UserUpdated,
    UserDeleted,
    ConnectionDenied,
    StreamProbeFailed,
    NotificationDeadLettered,
    ScheduledTaskFailed,
    AuthSignInSucceeded,
    AuthSignInFailed,
    AuthSignInThrottled,
    AuthPermissionDenied,
    /// Payload-free progress nudge, appended last so no existing kind's bit
    /// position shifts.
    RecordingProgress,
}

impl EventKind {
    /// Every kind, in declaration order.
    ///
    /// The mask type below indexes into this, so the order is load-bearing:
    /// it is the bit order, not just a listing.
    pub const ALL: [Self; 43] = [
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
        Self::RecordingChanged,
        Self::RecordingRulesChanged,
        Self::InputMetadataUpdatesCompleted,
        Self::InputMetadataUpdatesStarted,
        Self::InputMetadataUpdatesFailed,
        Self::DiskAlert,
        Self::ConfigReloadFailed,
        Self::PlaylistWatchChanged,
        Self::PlaylistGroupsChanged,
        Self::PlaylistWatchDisabled,
        Self::PlaylistWatchUnmatched,
        Self::RecordingStarted,
        Self::RecordingCompleted,
        Self::RecordingFailed,
        Self::ProviderAccountStatus,
        Self::ProviderAccountExpiring,
        Self::ProviderAccountExpired,
        Self::ProviderFetchFailed,
        Self::ProviderPoolExhausted,
        Self::ProviderPriorityFallback,
        Self::UserCreated,
        Self::UserUpdated,
        Self::UserDeleted,
        Self::ConnectionDenied,
        Self::StreamProbeFailed,
        Self::NotificationDeadLettered,
        Self::ScheduledTaskFailed,
        Self::AuthSignInSucceeded,
        Self::AuthSignInFailed,
        Self::AuthSignInThrottled,
        Self::AuthPermissionDenied,
        Self::RecordingProgress,
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
            Self::RecordingChanged
            | Self::RecordingProgress
            | Self::RecordingRulesChanged
            | Self::RecordingStarted
            | Self::RecordingCompleted
            | Self::RecordingFailed => Permission::RecordingRead,
            Self::PlaylistUpdate
            | Self::PlaylistUpdateProgress
            | Self::PlaylistWatchChanged
            | Self::PlaylistGroupsChanged
            | Self::PlaylistWatchDisabled
            | Self::PlaylistWatchUnmatched => Permission::PlaylistWrite,
            Self::LibraryScanProgress | Self::LibraryScanFailed => Permission::LibraryWrite,
            // Who may hear that an account was created is the same question
            // as who may list accounts.
            Self::UserCreated | Self::UserUpdated | Self::UserDeleted => Permission::UserRead,
            // Who was turned away is the same question as who signed in, and
            // strictly narrower than the system-wide read.
            Self::ConnectionDenied => Permission::UserRead,
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
            | Self::ProviderFetchFailed
            | Self::ProviderPoolExhausted
            | Self::ProviderPriorityFallback
            // Sits with the metadata-update events it is produced by.
            | Self::StreamProbeFailed
            | Self::NotificationDeadLettered
            | Self::ScheduledTaskFailed => Permission::SystemRead,
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
            Self::PlaylistUpdateProgress | Self::LibraryScanProgress | Self::RecordingProgress | Self::SystemInfoUpdate
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
    pub const fn is_latched(self) -> bool { matches!(self, Self::SystemInfoUpdate | Self::ActiveProvider) }

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
            Self::RecordingChanged => "recording.changed",
            Self::RecordingProgress => "recording.progress",
            Self::RecordingRulesChanged => "recording.rules.changed",
            Self::InputMetadataUpdatesCompleted => "metadata.update.completed",
            Self::InputMetadataUpdatesStarted => "metadata.update.started",
            Self::InputMetadataUpdatesFailed => "metadata.update.failed",
            Self::DiskAlert => "system.disk.alert",
            Self::ConfigReloadFailed => "config.reload.failed",
            Self::PlaylistWatchChanged => "playlist.watch.changed",
            Self::PlaylistGroupsChanged => "playlist.groups.changed",
            Self::PlaylistWatchDisabled => "playlist.watch.disabled",
            Self::PlaylistWatchUnmatched => "playlist.watch.unmatched",
            Self::RecordingStarted => "recording.started",
            Self::RecordingCompleted => "recording.completed",
            Self::RecordingFailed => "recording.failed",
            Self::ProviderAccountStatus => "provider.account.status",
            Self::ProviderAccountExpiring => "provider.account.expiring",
            Self::ProviderAccountExpired => "provider.account.expired",
            Self::ProviderFetchFailed => "provider.fetch.failed",
            Self::ProviderPoolExhausted => "provider.pool.exhausted",
            Self::ProviderPriorityFallback => "provider.priority.fallback",
            Self::UserCreated => "user.created",
            Self::UserUpdated => "user.updated",
            Self::UserDeleted => "user.deleted",
            Self::ConnectionDenied => "user.connection.denied",
            Self::StreamProbeFailed => "stream.probe.failed",
            Self::NotificationDeadLettered => "notification.dead_lettered",
            Self::ScheduledTaskFailed => "scheduled_task.failed",
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
