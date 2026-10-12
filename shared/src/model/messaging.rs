use crate::concat_string;
use std::sync::Arc;

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
    use super::{MsgKind, WatchChange, WatchChanges, WatchGroupItems, WatchItems};
    use std::sync::Arc;

    fn titles(names: &[&str]) -> Vec<Arc<str>> { names.iter().map(|n| Arc::from(*n)).collect() }

    fn sample_changes() -> WatchChanges {
        WatchChanges::new(
            "sports".to_string(),
            vec![
                WatchChange::GroupsAdded(WatchItems::complete(titles(&["Event Bundesliga"]))),
                WatchChange::GroupsRemoved(WatchItems::complete(titles(&["DE Kids"]))),
                WatchChange::ChannelsAdded(WatchGroupItems {
                    group: "DE Sport HD".into(),
                    items: WatchItems::complete(titles(&["Sky Sport 5"])),
                }),
            ],
        )
    }

    #[test]
    fn watch_changes_serialize_as_tagged_list() {
        let json = serde_json::to_value(sample_changes()).expect("serialize");
        assert_eq!(json["target"], "sports");
        assert_eq!(json["changes"][0]["kind"], "groups_added");
        assert_eq!(json["changes"][0]["items"][0], "Event Bundesliga");
        assert_eq!(json["changes"][2]["kind"], "channels_added");
        assert_eq!(json["changes"][2]["group"], "DE Sport HD");
        assert_eq!(json["changes"][2]["total"], 1);
        assert_eq!(json["changes"][2]["truncated"], false);
        let back: WatchChanges = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, sample_changes());
    }

    #[test]
    fn template_view_groups_changes_by_kind() {
        let changes = sample_changes();
        let view = serde_json::to_value(changes.template_view()).expect("serialize view");
        assert_eq!(view["groups_added"]["items"][0], "Event Bundesliga");
        assert_eq!(view["groups_removed"]["total"], 1);
        assert_eq!(view["channels_added"][0]["group"], "DE Sport HD");
        assert!(view.get("channels_removed").is_none(), "empty views must be absent");
        assert_eq!(view["changes"].as_array().map(Vec::len), Some(3));
    }

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

/// Every watch change one target refresh detected.
///
/// Lives here rather than in `tuliprox-core` because it is an event payload:
/// `EventMessage::PlaylistWatchChanged` carries it, and `shared` is the one
/// crate every emitter and every subscriber can name. One event per target
/// and refresh, so a reshuffle across many groups is one notification.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WatchChanges {
    pub target: String,
    /// Group changes first, then channel changes ordered by group title.
    pub changes: Vec<WatchChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchChange {
    ChannelsAdded(WatchGroupItems),
    ChannelsRemoved(WatchGroupItems),
    GroupsAdded(WatchItems),
    GroupsRemoved(WatchItems),
}

/// A sampled list of titles.
///
/// `items` is shorter than `total` when `truncated` is set, and empty when the
/// change was too large to list - but never anything except real titles. A
/// plugin reading the JSON cannot tell a synthesised "... 42 more" entry from a
/// channel actually named that, so the counts live beside the list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WatchItems {
    pub items: Vec<Arc<str>>,
    pub total: usize,
    pub truncated: bool,
}

impl WatchItems {
    /// A complete list, nothing sampled.
    #[must_use]
    pub fn complete(items: Vec<Arc<str>>) -> Self { Self { total: items.len(), items, truncated: false } }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WatchGroupItems {
    pub group: Arc<str>,
    #[serde(flatten)]
    pub items: WatchItems,
}

impl WatchChanges {
    #[must_use]
    pub fn new(target: String, changes: Vec<WatchChange>) -> Self { Self { target, changes } }

    /// The shape notification templates see under `watch`.
    #[must_use]
    pub fn template_view(&self) -> WatchTemplateView<'_> {
        let mut view = WatchTemplateView {
            target: &self.target,
            changes: &self.changes,
            groups_added: None,
            groups_removed: None,
            channels_added: Vec::new(),
            channels_removed: Vec::new(),
        };
        for change in &self.changes {
            match change {
                WatchChange::GroupsAdded(items) => view.groups_added = Some(items),
                WatchChange::GroupsRemoved(items) => view.groups_removed = Some(items),
                WatchChange::ChannelsAdded(group) => view.channels_added.push(group),
                WatchChange::ChannelsRemoved(group) => view.channels_removed.push(group),
            }
        }
        view
    }
}

/// `changes` regrouped by kind, so a template can say
/// `{{#each watch.groups_added.items}}` instead of filtering the raw list
/// with `eq`. Borrows from the payload; absent keys mean "none".
#[derive(Debug, serde::Serialize)]
pub struct WatchTemplateView<'a> {
    pub target: &'a str,
    pub changes: &'a [WatchChange],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups_added: Option<&'a WatchItems>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups_removed: Option<&'a WatchItems>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub channels_added: Vec<&'a WatchGroupItems>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub channels_removed: Vec<&'a WatchGroupItems>,
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
