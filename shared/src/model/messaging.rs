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
        }
    }

    pub fn template_filename(&self, prefix: &str) -> String { concat_string!(prefix, "_", self.wire_name(), ".templ") }
}

impl fmt::Display for MsgKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MsgKind::Info => "Info",
            MsgKind::Stats => "Stats",
            MsgKind::Error => "Error",
            MsgKind::Watch => "Watch",
            MsgKind::DiskAlert => "DiskAlert",
        };
        write!(f, "{s}")
    }
}

impl FromStr for MsgKind {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq_ignore_ascii_case("info") {
            Ok(Self::Info)
        } else if s.eq_ignore_ascii_case("stats") {
            Ok(Self::Stats)
        } else if s.eq_ignore_ascii_case("error") {
            Ok(Self::Error)
        } else if s.eq_ignore_ascii_case("watch") {
            Ok(Self::Watch)
        } else if s.eq_ignore_ascii_case("disk_alert") {
            Ok(Self::DiskAlert)
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
}
