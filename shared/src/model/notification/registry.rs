use super::{EventDescriptor, EventId, Severity};

/// Fallback for an id read back from a persisted outbox that this build
/// does not know. Lets an outbox written by a newer version round-trip
/// through an older one instead of failing the whole file.
pub const UNKNOWN: EventId = EventId::new("unknown");

// ---- system ---------------------------------------------------------
pub const SYSTEM_INFO: EventId = EventId::new("system.info");
pub const SYSTEM_ERROR: EventId = EventId::new("system.error");
pub const SYSTEM_DISK_ALERT: EventId = EventId::new("system.disk.alert");
pub const SYSTEM_STARTED: EventId = EventId::new("system.started");
pub const SYSTEM_SHUTDOWN: EventId = EventId::new("system.shutdown");

// ---- playlist -------------------------------------------------------
pub const PLAYLIST_UPDATE_COMPLETED: EventId = EventId::new("playlist.update.completed");
pub const PLAYLIST_UPDATE_FAILED: EventId = EventId::new("playlist.update.failed");
pub const PLAYLIST_WATCH_CHANGED: EventId = EventId::new("playlist.watch.changed");
pub const PLAYLIST_GROUPS_CHANGED: EventId = EventId::new("playlist.groups.changed");
pub const PLAYLIST_WATCH_DISABLED: EventId = EventId::new("playlist.watch.disabled");
pub const PLAYLIST_WATCH_UNMATCHED: EventId = EventId::new("playlist.watch.unmatched");

// ---- recording ------------------------------------------------------
pub const RECORDING_STARTED: EventId = EventId::new("recording.started");
pub const RECORDING_COMPLETED: EventId = EventId::new("recording.completed");
pub const RECORDING_FAILED: EventId = EventId::new("recording.failed");

// ---- provider -------------------------------------------------------
pub const PROVIDER_ACCOUNT_STATUS: EventId = EventId::new("provider.account.status_changed");
pub const PROVIDER_ACCOUNT_EXPIRING: EventId = EventId::new("provider.account.expiring");
pub const PROVIDER_ACCOUNT_EXPIRED: EventId = EventId::new("provider.account.expired");
pub const PROVIDER_FETCH_FAILED: EventId = EventId::new("provider.fetch.failed");
pub const PROVIDER_POOL_EXHAUSTED: EventId = EventId::new("provider.pool.exhausted");
pub const PROVIDER_PRIORITY_FALLBACK: EventId = EventId::new("provider.priority.fallback");

// ---- config ---------------------------------------------------------
pub const CONFIG_CHANGED: EventId = EventId::new("config.changed");
pub const CONFIG_RELOAD_FAILED: EventId = EventId::new("config.reload_failed");

// ---- library --------------------------------------------------------
pub const LIBRARY_SCAN_COMPLETED: EventId = EventId::new("library.scan.completed");
pub const LIBRARY_SCAN_FAILED: EventId = EventId::new("library.scan.failed");

// ---- metadata -------------------------------------------------------
pub const METADATA_UPDATE_STARTED: EventId = EventId::new("metadata.update.started");
pub const METADATA_UPDATE_COMPLETED: EventId = EventId::new("metadata.update.completed");
pub const METADATA_UPDATE_FAILED: EventId = EventId::new("metadata.update.failed");

// ---- users and connections ------------------------------------------
/// High frequency. Subscribe deliberately.
pub const USER_CONNECTION_CHANGED: EventId = EventId::new("user.connection.changed");
/// High frequency. Subscribe deliberately.
pub const PROVIDER_CONNECTIONS_CHANGED: EventId = EventId::new("provider.connections.changed");
/// A user was refused a connection because their limits were full. Not
/// high frequency: reported per refusal, and a refusal is not routine.
pub const USER_CONNECTION_DENIED: EventId = EventId::new("user.connection.denied");

// ---- user accounts ---------------------------------------------------
pub const USER_CREATED: EventId = EventId::new("user.created");
pub const USER_UPDATED: EventId = EventId::new("user.updated");
pub const USER_DELETED: EventId = EventId::new("user.deleted");

// ---- authentication --------------------------------------------------
pub const AUTH_SIGN_IN_SUCCEEDED: EventId = EventId::new("auth.sign_in.succeeded");
pub const AUTH_SIGN_IN_FAILED: EventId = EventId::new("auth.sign_in.failed");
pub const AUTH_SIGN_IN_THROTTLED: EventId = EventId::new("auth.sign_in.throttled");
pub const AUTH_PERMISSION_DENIED: EventId = EventId::new("auth.permission.denied");

// ---- streams ---------------------------------------------------------
pub const STREAM_PROBE_FAILED: EventId = EventId::new("stream.probe.failed");

// ---- dvr ------------------------------------------------------------
pub const RECORDING_QUEUE_CHANGED: EventId = EventId::new("recording.queue.changed");
pub const RECORDING_RULES_CHANGED: EventId = EventId::new("recording.rules.changed");

// ---- scheduler -------------------------------------------------------
pub const SCHEDULED_TASK_FAILED: EventId = EventId::new("scheduled_task.failed");

// ---- notification self-reporting ------------------------------------
/// A notification was permanently lost. Must never route back through
/// the channel that dropped it.
pub const NOTIFICATION_DEAD_LETTERED: EventId = EventId::new("notification.dead_lettered");

/// Every registered event, in display order.
pub const ALL: &[EventDescriptor] = &[
        EventDescriptor { id: SYSTEM_INFO, severity: Severity::Info, description: "A general informational message." },
        EventDescriptor { id: SYSTEM_ERROR, severity: Severity::Error, description: "A general error message." },
        EventDescriptor {
            id: SYSTEM_DISK_ALERT,
            severity: Severity::Warn,
            description: "Disk usage crossed the warn or critical threshold.",
        },
        EventDescriptor {
            id: SYSTEM_STARTED,
            severity: Severity::Info,
            description: "The server finished starting up.",
        },
        EventDescriptor {
            id: SYSTEM_SHUTDOWN,
            severity: Severity::Info,
            description: "The server is shutting down cleanly.",
        },
        EventDescriptor {
            id: PLAYLIST_UPDATE_COMPLETED,
            severity: Severity::Info,
            description: "A playlist update finished; carries per-source statistics.",
        },
        EventDescriptor {
            id: PLAYLIST_UPDATE_FAILED,
            severity: Severity::Error,
            description: "A playlist update failed.",
        },
        EventDescriptor {
            id: PLAYLIST_WATCH_CHANGED,
            severity: Severity::Info,
            description: "Channels were added to or removed from a watched group.",
        },
        EventDescriptor {
            id: PLAYLIST_GROUPS_CHANGED,
            severity: Severity::Info,
            description: "Groups were added to or removed from a target.",
        },
        EventDescriptor {
            id: PLAYLIST_WATCH_DISABLED,
            severity: Severity::Warn,
            description: "A target's watch configuration is set but not working.",
        },
        EventDescriptor {
            id: PLAYLIST_WATCH_UNMATCHED,
            severity: Severity::Warn,
            description: "Watch patterns matched no group in the refreshed playlist.",
        },
        EventDescriptor { id: RECORDING_STARTED, severity: Severity::Info, description: "A recording started." },
        EventDescriptor { id: RECORDING_COMPLETED, severity: Severity::Info, description: "A recording completed." },
        EventDescriptor { id: RECORDING_FAILED, severity: Severity::Error, description: "A recording failed." },
        EventDescriptor {
            id: PROVIDER_ACCOUNT_STATUS,
            severity: Severity::Warn,
            description: "A provider reported a changed account status.",
        },
        EventDescriptor {
            id: PROVIDER_ACCOUNT_EXPIRING,
            severity: Severity::Warn,
            description: "A provider account is approaching its expiry date.",
        },
        EventDescriptor {
            id: PROVIDER_ACCOUNT_EXPIRED,
            severity: Severity::Error,
            description: "A provider account has expired.",
        },
        EventDescriptor {
            id: PROVIDER_FETCH_FAILED,
            severity: Severity::Error,
            description: "An input's playlist could not be fetched.",
        },
        EventDescriptor {
            id: PROVIDER_POOL_EXHAUSTED,
            severity: Severity::Warn,
            description: "Every provider behind an input was at capacity.",
        },
        EventDescriptor {
            id: PROVIDER_PRIORITY_FALLBACK,
            severity: Severity::Warn,
            description: "An input started being served from a different provider priority group.",
        },
        EventDescriptor {
            id: CONFIG_CHANGED,
            severity: Severity::Info,
            description: "A configuration file was changed and reloaded.",
        },
        EventDescriptor {
            id: CONFIG_RELOAD_FAILED,
            severity: Severity::Error,
            description: "A configuration file changed but could not be reloaded.",
        },
        EventDescriptor {
            id: LIBRARY_SCAN_COMPLETED,
            severity: Severity::Info,
            description: "A local library scan finished.",
        },
        EventDescriptor {
            id: LIBRARY_SCAN_FAILED,
            severity: Severity::Error,
            description: "A local library scan could not complete.",
        },
        EventDescriptor {
            id: METADATA_UPDATE_STARTED,
            severity: Severity::Info,
            description: "A metadata update started for an input.",
        },
        EventDescriptor {
            id: METADATA_UPDATE_COMPLETED,
            severity: Severity::Info,
            description: "A metadata update finished for an input.",
        },
        EventDescriptor {
            id: METADATA_UPDATE_FAILED,
            severity: Severity::Error,
            description: "A metadata update cycle ended with tasks it could not finish.",
        },
        EventDescriptor {
            id: USER_CONNECTION_CHANGED,
            severity: Severity::Info,
            description: "A user connected or disconnected. High frequency - subscribe deliberately.",
        },
        EventDescriptor {
            id: PROVIDER_CONNECTIONS_CHANGED,
            severity: Severity::Info,
            description: "A provider's active connection count changed. High frequency - subscribe deliberately.",
        },
        EventDescriptor {
            id: USER_CONNECTION_DENIED,
            severity: Severity::Warn,
            description: "A user was refused a connection because their limits were full.",
        },
        EventDescriptor {
            id: USER_CREATED,
            severity: Severity::Info,
            description: "An API-proxy user account was created.",
        },
        EventDescriptor {
            id: USER_UPDATED,
            severity: Severity::Info,
            description: "An API-proxy user account was changed.",
        },
        EventDescriptor {
            id: USER_DELETED,
            severity: Severity::Warn,
            description: "An API-proxy user account was deleted.",
        },
        EventDescriptor {
            id: AUTH_SIGN_IN_SUCCEEDED,
            severity: Severity::Info,
            description: "A principal signed in and was issued a token.",
        },
        EventDescriptor {
            id: AUTH_SIGN_IN_FAILED,
            severity: Severity::Warn,
            description: "A sign-in was rejected. Deduplicated per principal and address, so a password-guessing run notifies once rather than per attempt.",
        },
        EventDescriptor {
            id: AUTH_SIGN_IN_THROTTLED,
            severity: Severity::Warn,
            description: "A sign-in was refused without checking credentials because the caller is backing off after repeated failures.",
        },
        EventDescriptor {
            id: AUTH_PERMISSION_DENIED,
            severity: Severity::Warn,
            description: "An authenticated principal asked for something its permissions do not cover.",
        },
        EventDescriptor {
            id: STREAM_PROBE_FAILED,
            severity: Severity::Warn,
            description: "A stream probe returned no metadata; the stream may be dead. Deduplicated per input, so a provider outage notifies once rather than per channel.",
        },
        EventDescriptor {
            id: RECORDING_QUEUE_CHANGED,
            severity: Severity::Info,
            description: "The recording queue changed.",
        },
        EventDescriptor {
            id: RECORDING_RULES_CHANGED,
            severity: Severity::Info,
            description: "The recording rule set changed.",
        },
        EventDescriptor {
            id: SCHEDULED_TASK_FAILED,
            severity: Severity::Error,
            description: "A scheduled task could not complete.",
        },
        EventDescriptor {
            id: NOTIFICATION_DEAD_LETTERED,
            severity: Severity::Error,
            description: "A notification was permanently undeliverable and has been dropped.",
        },
    ];

/// Look up the descriptor for an id.
#[must_use]
pub fn describe(id: EventId) -> Option<&'static EventDescriptor> { ALL.iter().find(|d| d.id == id) }

/// Default severity for an id; `Info` for anything unregistered.
#[must_use]
pub fn default_severity(id: EventId) -> Severity { describe(id).map_or(Severity::Info, |d| d.severity) }
