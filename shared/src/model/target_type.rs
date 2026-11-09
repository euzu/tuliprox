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
    /// A custom output format rendered by a plugin (`type: plugin` + `plugin: <id>`, plan §18 Q6).
    /// Unit variant: the plugin id lives in `PluginTargetOutputDto::plugin`.
    Plugin,
}

impl TargetType {
    /// Lowercase wire name, identical to the `serde` representation (`xtream`, `m3u`, `strm`, `hdhomerun`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::M3u => "m3u",
            Self::Xtream => "xtream",
            Self::Strm => "strm",
            Self::HdHomeRun => "hdhomerun",
            Self::Plugin => "plugin",
        }
    }

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
            Self::Xtream => {
                TargetCapabilities { supports_filter: true, supports_epg: true, supports_memory_cache: true }
            }
            Self::M3u => TargetCapabilities { supports_filter: true, supports_epg: true, supports_memory_cache: true },
            Self::Strm => {
                TargetCapabilities { supports_filter: true, supports_epg: false, supports_memory_cache: false }
            }
            Self::HdHomeRun => {
                TargetCapabilities { supports_filter: false, supports_epg: false, supports_memory_cache: false }
            }
            // The host applies the output filter before it hands the items to `render-batch`, so the
            // filter is honored. The output-generator interface carries channels only, with no EPG, and
            // the rendered file is not a playlist db the memory cache could serve.
            Self::Plugin => {
                TargetCapabilities { supports_filter: true, supports_epg: false, supports_memory_cache: false }
            }
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

#[cfg(test)]
mod tests {
    use super::TargetType;
    use strum::IntoEnumIterator;

    #[test]
    fn as_str_matches_the_serde_name_of_every_variant() {
        for target_type in TargetType::iter() {
            assert_eq!(serde_json::to_value(target_type).unwrap(), serde_json::json!(target_type.as_str()));
        }
    }

    #[test]
    fn plugin_row_is_explicit_on_all_three_flags() {
        let capabilities = TargetType::Plugin.capabilities();
        assert!(capabilities.supports_filter);
        assert!(!capabilities.supports_epg);
        assert!(!capabilities.supports_memory_cache);
        assert_eq!(TargetType::Plugin.as_str(), "plugin");
    }

    #[test]
    fn plugin_spells_plugin_in_serde_and_strum() {
        assert_eq!(serde_json::to_value(TargetType::Plugin).unwrap(), serde_json::json!("plugin"));
        assert_eq!(serde_json::from_value::<TargetType>(serde_json::json!("plugin")).unwrap(), TargetType::Plugin);
        assert_eq!(TargetType::Plugin.to_string(), "Plugin");
        assert_eq!("plugin".parse::<TargetType>().unwrap(), TargetType::Plugin);
    }
}
