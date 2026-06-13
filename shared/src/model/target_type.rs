use strum_macros::{Display, EnumIter, EnumString};

#[derive(
    Debug, Copy, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, EnumIter, Display, EnumString,
)]
#[strum(serialize_all = "PascalCase", ascii_case_insensitive)]
#[serde(rename_all = "lowercase")]
pub enum TargetType {
    M3u,
    Xtream,
    Strm,
    HdHomeRun,
}

impl TargetType {
    /// Single source of truth for the categorical capabilities of an output
    /// format.
    ///
    /// Adding a new [`TargetType`] variant forces an arm here (the match is
    /// exhaustive), and the filtering / EPG / memory-cache sites derive from
    /// this table instead of each re-deciding per variant — so a new format can
    /// no longer silently inherit a blank "no-op" arm in one of those places.
    #[must_use]
    pub const fn capabilities(self) -> TargetCapabilities {
        match self {
            Self::Xtream => TargetCapabilities { supports_filter: true, supports_epg: true, supports_memory_cache: true },
            Self::M3u => TargetCapabilities { supports_filter: true, supports_epg: true, supports_memory_cache: true },
            Self::Strm => TargetCapabilities { supports_filter: true, supports_epg: false, supports_memory_cache: false },
            Self::HdHomeRun => TargetCapabilities { supports_filter: false, supports_epg: false, supports_memory_cache: false },
        }
    }

    /// Whether this output format produces an EPG.
    #[must_use]
    pub const fn supports_epg(self) -> bool { self.capabilities().supports_epg }

    /// Whether this output format honors a playlist filter.
    #[must_use]
    pub const fn supports_filter(self) -> bool { self.capabilities().supports_filter }

    /// Whether this output format can be served from the in-memory playlist cache.
    #[must_use]
    pub const fn supports_memory_cache(self) -> bool { self.capabilities().supports_memory_cache }
}

/// Categorical capabilities of a [`TargetType`], declared once in
/// [`TargetType::capabilities`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TargetCapabilities {
    /// Whether the format honors a playlist filter.
    pub supports_filter: bool,
    /// Whether the format produces an EPG.
    pub supports_epg: bool,
    /// Whether the format can be served from the in-memory playlist cache.
    pub supports_memory_cache: bool,
}
