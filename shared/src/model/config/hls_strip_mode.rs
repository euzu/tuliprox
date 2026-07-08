#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HlsStripMode {
    #[default]
    Segments,
    Seconds,
}

crate::impl_str_enum!(HlsStripMode, "HLS strip mode",
    Segments => "segments",
    Seconds => "seconds",
);
