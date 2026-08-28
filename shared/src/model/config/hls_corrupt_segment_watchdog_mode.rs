#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HlsCorruptSegmentWatchdogMode {
    #[default]
    Off,
    DetectOnly,
    Sanitize,
    Diagnostic,
}

impl HlsCorruptSegmentWatchdogMode {
    /// True when the watchdog is configured to take any action (sanitize or
    /// emit diagnostics). `Off` and `DetectOnly` are observation-only.
    pub const fn is_enabled(self) -> bool { !matches!(self, Self::Off) }

    /// Stable lowercase log label — same canonical form as the `Display` impl
    /// so log readers can correlate the two.
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DetectOnly => "detect_only",
            Self::Sanitize => "sanitize",
            Self::Diagnostic => "diagnostic",
        }
    }
}

crate::impl_str_enum!(HlsCorruptSegmentWatchdogMode, "HLS corrupt segment watchdog mode",
    Off => "off",
    DetectOnly => "detect_only",
    Sanitize => "sanitize",
    Diagnostic => "diagnostic",
);
