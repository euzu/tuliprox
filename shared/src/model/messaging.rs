use crate::{concat_string, error::TuliproxError};
use std::{fmt, str::FromStr};

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub enum MsgKind {
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "stats")]
    Stats,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "watch")]
    Watch,
    #[serde(rename = "disk_alert")]
    DiskAlert,
    /// A recording started.
    #[serde(rename = "recording_started")]
    RecordingStarted,
    /// A recording completed.
    #[serde(rename = "recording_completed")]
    RecordingCompleted,
    /// A recording failed.
    #[serde(rename = "recording_failed")]
    RecordingFailed,
}
impl MsgKind {
    /// Stable snake_case wire name. Matches the `#[serde(rename = "...")]`
    /// annotation on the variant and is used for config keys, template
    /// filenames, and any other text format that needs a stable identifier
    /// independent of the Rust variant name.
    pub fn wire_name(&self) -> &'static str {
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

impl fmt::Display for MsgKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MsgKind::Info => "Info",
            MsgKind::Stats => "Stats",
            MsgKind::Error => "Error",
            MsgKind::Watch => "Watch",
            MsgKind::DiskAlert => "DiskAlert",
            MsgKind::RecordingStarted => "RecordingStarted",
            MsgKind::RecordingCompleted => "RecordingCompleted",
            MsgKind::RecordingFailed => "RecordingFailed",
        };
        write!(f, "{s}")
    }
}

impl FromStr for MsgKind {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        // Accepts both the snake_case wire name and the CamelCase variant
        // name so values produced by `Display`/`to_string` round-trip.
        if s.eq_ignore_ascii_case("info") {
            Ok(Self::Info)
        } else if s.eq_ignore_ascii_case("stats") {
            Ok(Self::Stats)
        } else if s.eq_ignore_ascii_case("error") {
            Ok(Self::Error)
        } else if s.eq_ignore_ascii_case("watch") {
            Ok(Self::Watch)
        } else if s.eq_ignore_ascii_case("disk_alert") || s.eq_ignore_ascii_case("diskalert") {
            Ok(Self::DiskAlert)
        } else if s.eq_ignore_ascii_case("recording_started") || s.eq_ignore_ascii_case("recordingstarted") {
            Ok(Self::RecordingStarted)
        } else if s.eq_ignore_ascii_case("recording_completed") || s.eq_ignore_ascii_case("recordingcompleted") {
            Ok(Self::RecordingCompleted)
        } else if s.eq_ignore_ascii_case("recording_failed") || s.eq_ignore_ascii_case("recordingfailed") {
            Ok(Self::RecordingFailed)
        } else {
            Err(TuliproxError::Config(format!("Unknown MsgKind: {s}")))
        }
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
