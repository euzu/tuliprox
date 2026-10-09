use super::{registry, EventId};
use std::fmt;

impl EventId {
    /// Declare an event id. `const` so the registry is built at compile time.
    #[must_use]
    pub const fn new(id: &'static str) -> Self { Self(id) }

    #[must_use]
    pub const fn as_str(&self) -> &'static str { self.0 }

    /// The `domain` part - everything before the first dot.
    ///
    /// Used to group events in the UI and to answer "does this subscription
    /// cover the whole domain".
    #[must_use]
    pub fn domain(&self) -> &'static str {
        match self.0.split_once('.') {
            Some((domain, _)) => domain,
            None => self.0,
        }
    }

    /// Filename-safe form: dots become underscores.
    ///
    /// `recording.completed` yields `recording_completed`, which is exactly
    /// the legacy template filename for that kind - so recording templates
    /// are discovered by the canonical id with no alias lookup needed.
    #[must_use]
    pub fn file_stem(&self) -> String { self.0.replace('.', "_") }

    /// `{prefix}_{file_stem}.templ`, the on-disk template name.
    #[must_use]
    pub fn template_filename(&self, prefix: &str) -> String { format!("{prefix}_{}.templ", self.file_stem()) }

    /// Resolve a wire string to a known event id.
    ///
    /// Accepts the canonical dotted id and every legacy `MsgKind` wire
    /// name. Returns `None` for an id that is not in the registry, which is
    /// what lets config validation reject a typo instead of silently
    /// subscribing to an event that will never fire.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        if let Some((_, id)) = LEGACY_ALIASES.iter().find(|(legacy, _)| legacy.eq_ignore_ascii_case(s)) {
            return Some(*id);
        }
        registry::ALL.iter().find(|d| d.id.0.eq_ignore_ascii_case(s)).map(|d| d.id)
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.0) }
}

impl serde::Serialize for EventId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_str(self.0) }
}

impl<'de> serde::Deserialize<'de> for EventId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        // An unknown id in a persisted outbox entry must not fail the whole
        // file - see `UNKNOWN`. Config validation rejects typos separately,
        // where an error message can actually reach the user.
        Ok(Self::from_wire(&raw).unwrap_or(registry::UNKNOWN))
    }
}

/// How much the operator is expected to care.
///
/// Ordered, so a channel can subscribe with `min_severity` and get
/// everything at or above it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
    strum_macros::Display,
    strum_macros::EnumString,
    strum_macros::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum Severity {
    /// Something finished normally. Safe to route nowhere.
    #[default]
    Info,
    /// Degraded but self-correcting, or a threshold approached.
    Warn,
    /// An operation failed. Someone should look, eventually.
    Error,
    /// Service is impaired now. Worth waking someone.
    Critical,
}

impl Severity {
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }

    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> { s.parse().ok() }
}

/// One entry in the event registry.
#[derive(Debug, Clone, Copy)]
pub struct EventDescriptor {
    pub id: EventId,
    /// Severity the emitter uses unless it overrides per-occurrence.
    pub severity: Severity,
    /// One line, rendered into the docs table and the UI picker.
    pub description: &'static str,
}

/// Legacy `MsgKind` wire name -> canonical id.
///
/// Every entry here is load-bearing for an existing `config.yml`. Removing
/// one silently stops honouring a `notify_on` line that used to work, so
/// entries are append-only.
pub const LEGACY_ALIASES: &[(&str, EventId)] = &[
    ("info", registry::SYSTEM_INFO),
    ("stats", registry::PLAYLIST_UPDATE_COMPLETED),
    ("error", registry::SYSTEM_ERROR),
    ("watch", registry::PLAYLIST_WATCH_CHANGED),
    ("disk_alert", registry::SYSTEM_DISK_ALERT),
    ("diskalert", registry::SYSTEM_DISK_ALERT),
    ("recording_started", registry::RECORDING_STARTED),
    ("recordingstarted", registry::RECORDING_STARTED),
    ("recording_completed", registry::RECORDING_COMPLETED),
    ("recordingcompleted", registry::RECORDING_COMPLETED),
    ("recording_failed", registry::RECORDING_FAILED),
    ("recordingfailed", registry::RECORDING_FAILED),
];
