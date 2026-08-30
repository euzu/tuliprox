use crate::concat_string;

#[derive(
    Debug,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Hash,
    strum_macros::Display,
    strum_macros::EnumString,
    strum_macros::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(ascii_case_insensitive)]
pub enum MsgKind {
    Info,
    Stats,
    Error,
    Watch,
    #[strum(serialize = "DiskAlert", serialize = "disk_alert", serialize = "diskalert")]
    DiskAlert,
    /// A recording started.
    #[strum(serialize = "RecordingStarted", serialize = "recording_started", serialize = "recordingstarted")]
    RecordingStarted,
    /// A recording completed.
    #[strum(serialize = "RecordingCompleted", serialize = "recording_completed", serialize = "recordingcompleted")]
    RecordingCompleted,
    /// A recording failed.
    #[strum(serialize = "RecordingFailed", serialize = "recording_failed", serialize = "recordingfailed")]
    RecordingFailed,
}
impl MsgKind {
    /// Stable `snake_case` wire name. Matches the `#[serde(rename = "...")]`
    /// annotation on the variant and is used for config keys, template
    /// filenames, and any other text format that needs a stable identifier
    /// independent of the Rust variant name.
    pub const fn wire_name(&self) -> &'static str {
        match self {
            MsgKind::Info => "info",
            MsgKind::Stats => "stats",
            MsgKind::Error => "error",
            MsgKind::Watch => "watch",
            MsgKind::DiskAlert => "disk_alert",
            MsgKind::RecordingStarted => "recording_started",
            MsgKind::RecordingCompleted => "recording_completed",
            MsgKind::RecordingFailed => "recording_failed",
        }
    }

    pub fn template_filename(&self, prefix: &str) -> String { concat_string!(prefix, "_", self.wire_name(), ".templ") }

    /// `true` for the recording lifecycle kinds.
    pub fn is_recording_lifecycle(&self) -> bool {
        matches!(self, MsgKind::RecordingStarted | MsgKind::RecordingCompleted | MsgKind::RecordingFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::MsgKind;

    #[test]
    fn wire_name_matches_serde_rename() {
        assert_eq!(MsgKind::Info.wire_name(), "info");
        assert_eq!(MsgKind::Stats.wire_name(), "stats");
        assert_eq!(MsgKind::Error.wire_name(), "error");
        assert_eq!(MsgKind::Watch.wire_name(), "watch");
        assert_eq!(MsgKind::DiskAlert.wire_name(), "disk_alert");
    }

    #[test]
    fn template_filename_uses_snake_case_wire_name() {
        // Regression test: `DiskAlert` previously resolved to
        // `telegram_diskalert.templ` instead of `telegram_disk_alert.templ`,
        // breaking on-disk template auto-discovery for the new variant.
        assert_eq!(MsgKind::Info.template_filename("telegram"), "telegram_info.templ");
        assert_eq!(MsgKind::Stats.template_filename("telegram"), "telegram_stats.templ");
        assert_eq!(MsgKind::Error.template_filename("telegram"), "telegram_error.templ");
        assert_eq!(MsgKind::Watch.template_filename("telegram"), "telegram_watch.templ");
        assert_eq!(MsgKind::DiskAlert.template_filename("telegram"), "telegram_disk_alert.templ");
        assert_eq!(MsgKind::DiskAlert.template_filename("discord"), "discord_disk_alert.templ");
        assert_eq!(MsgKind::DiskAlert.template_filename("rest"), "rest_disk_alert.templ");
        assert_eq!(MsgKind::DiskAlert.template_filename("pushover"), "pushover_disk_alert.templ");
    }

    #[test]
    fn display_round_trips_through_from_str() {
        // Regression test: `Display` emitted `DiskAlert` while `FromStr`
        // only accepted `disk_alert`, so the frontend notify_on radio
        // group silently dropped the selection on the way back.
        let kinds = [
            MsgKind::Info,
            MsgKind::Stats,
            MsgKind::Error,
            MsgKind::Watch,
            MsgKind::DiskAlert,
            MsgKind::RecordingStarted,
            MsgKind::RecordingCompleted,
            MsgKind::RecordingFailed,
        ];
        for kind in kinds {
            assert!(
                kind.to_string().parse::<MsgKind>().is_ok_and(|parsed| parsed == kind),
                "round-trip failed for {kind}"
            );
        }
    }
}

/// Group membership changes a target's `watch` config detected on refresh.
///
/// Lives here rather than in `tuliprox-core` because it is an event payload:
/// `EventMessage::PlaylistWatchChanged` carries it, and `shared` is the one
/// crate every emitter and every subscriber can name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WatchChanges {
    pub target: String,
    pub group: String,
    /// Channel titles added, as a sample. Shorter than [`Self::added_total`]
    /// when [`Self::truncated`] is set, and empty when the change was too
    /// large to list at all - but never anything except channel titles.
    pub added: Vec<String>,
    /// Channel titles removed. Same sampling rule as [`Self::added`].
    pub removed: Vec<String>,
    /// How many channels were added, whatever the list above carries.
    ///
    /// The lists used to absorb their own truncation notice: a synthesised
    /// `"... 42 more added entries omitted"` pushed in beside real channel
    /// titles. That was legible to the text template and to nothing else -
    /// [`crate::model::EventMessage::payload`] serialises this struct straight
    /// to JSON for plugins, and a plugin cannot tell a sentinel from a channel
    /// actually named that. The counts live here so the lists stay honest.
    #[serde(default)]
    pub added_total: usize,
    /// How many channels were removed. See [`Self::added_total`].
    #[serde(default)]
    pub removed_total: usize,
    /// Whether the lists are a sample rather than the whole change.
    #[serde(default)]
    pub truncated: bool,
}

impl WatchChanges {
    /// A complete change set, nothing sampled.
    ///
    /// The totals follow the lists, so a caller that is not truncating cannot
    /// get them out of step.
    #[must_use]
    pub fn new(target: String, group: String, added: Vec<String>, removed: Vec<String>) -> Self {
        Self { target, group, added_total: added.len(), removed_total: removed.len(), added, removed, truncated: false }
    }
}

/// One recording's lifecycle transition.
///
/// `event` says which transition: `MsgKind::Recording{Started,Completed,Failed}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordingLifecycleMessage {
    pub event: MsgKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub programme_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

/// Which provider-account transition an event describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderAccountState {
    /// The account left the `Active`/`Trial` states.
    StatusChanged,
    /// The account expires within three days.
    Expiring,
    /// The account has expired.
    Expired,
}

/// A provider account changing state, as observed during an Xtream login.
///
/// Carries the fields the notification templates already render, so moving
/// this onto the bus changes nothing an operator receives.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderAccountEvent {
    pub state: ProviderAccountState,
    pub username: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Human-readable summary, used as both notification title and body.
    pub message: String,
}

impl ProviderAccountEvent {
    /// The suppression key for this account transition.
    ///
    /// These are re-evaluated on every playlist refresh; without it an
    /// expiring account would notify on each one for the three days before
    /// expiry.
    #[must_use]
    pub fn dedup_key(&self) -> String {
        let prefix = match self.state {
            ProviderAccountState::StatusChanged => "provider.account.status",
            ProviderAccountState::Expiring => "provider.account.expiring",
            ProviderAccountState::Expired => "provider.account.expired",
        };
        format!("{prefix}:{}:{}", self.provider, self.username)
    }
}

/// A config file that failed to reload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConfigReloadFailure {
    /// The formatted path list the watcher reported.
    pub paths: String,
    pub error: String,
}
