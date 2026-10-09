use crate::{
    model::ClusterFlags,
    utils::{arc_str_option_serde, is_blank_optional_arc_str},
};
use std::sync::Arc;
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

#[derive(
    Debug,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Default,
    EnumIter,
    Display,
    EnumString,
    AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    #[default]
    M3u,
    Xtream,
    M3uBatch,
    XtreamBatch,
    Stalker,
    StalkerBatch,
    Library,
    Emby,
    Jellyfin,
    Plex,
    Staged,
}

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, Display, EnumString)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StagedInputType {
    #[default]
    M3u,
    Xtream,
}

impl StagedInputType {
    pub fn is_default(value: &Self) -> bool { matches!(value, Self::M3u) }

    pub const fn input_type(self) -> InputType {
        match self {
            Self::M3u => InputType::M3u,
            Self::Xtream => InputType::Xtream,
        }
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputStagedDto {
    #[serde(
        default,
        alias = "provider",
        skip_serializing_if = "is_blank_optional_arc_str",
        with = "arc_str_option_serde"
    )]
    pub for_input: Option<Arc<str>>,
    #[serde(default)]
    pub clusters: ClusterFlags,
}

impl InputType {
    pub fn is_xtream(&self) -> bool { matches!(self, Self::Xtream | Self::XtreamBatch) }
    pub fn is_m3u(&self) -> bool { matches!(self, Self::M3u | Self::M3uBatch) }
    pub fn is_stalker(&self) -> bool { matches!(self, Self::Stalker | Self::StalkerBatch) }
    pub fn uses_standard_input_url(&self) -> bool {
        matches!(
            self,
            Self::M3u | Self::Xtream | Self::M3uBatch | Self::XtreamBatch | Self::Stalker | Self::StalkerBatch
        )
    }
    pub fn is_library(&self) -> bool { matches!(self, Self::Library) }
    pub fn is_media_server(&self) -> bool { matches!(self, Self::Emby | Self::Jellyfin | Self::Plex) }
    pub fn is_batch(&self) -> bool { matches!(self, Self::M3uBatch | Self::XtreamBatch | Self::StalkerBatch) }
    pub fn is_staged(&self) -> bool { matches!(self, Self::Staged) }

    /// Single source of truth for the categorical behavior of an input type.
    ///
    /// Adding a new [`InputType`] variant forces an arm here (the match is
    /// exhaustive), and every site that consumes [`InputCapabilities`] —
    /// persistence/load routing, probe requirements, the custom-provider
    /// endpoint gate — stays in sync automatically instead of relying on a
    /// parallel `match` somewhere else that is easy to forget.
    #[must_use]
    pub const fn capabilities(self) -> InputCapabilities {
        match self {
            Self::M3u | Self::M3uBatch => InputCapabilities {
                persistence: InputPersistence::M3u,
                requires_provider_connection_for_probe: true,
                served_on_custom_provider_endpoint: true,
            },
            Self::Xtream | Self::XtreamBatch => InputCapabilities {
                persistence: InputPersistence::Xtream,
                requires_provider_connection_for_probe: true,
                served_on_custom_provider_endpoint: true,
            },
            Self::Stalker | Self::StalkerBatch => InputCapabilities {
                persistence: InputPersistence::Stalker,
                requires_provider_connection_for_probe: true,
                served_on_custom_provider_endpoint: true,
            },
            Self::Library => InputCapabilities {
                persistence: InputPersistence::Library,
                requires_provider_connection_for_probe: false,
                served_on_custom_provider_endpoint: false,
            },
            Self::Emby | Self::Jellyfin | Self::Plex => InputCapabilities {
                persistence: InputPersistence::MediaServer,
                requires_provider_connection_for_probe: false,
                served_on_custom_provider_endpoint: false,
            },
            Self::Staged => InputCapabilities {
                persistence: InputPersistence::M3u,
                requires_provider_connection_for_probe: false,
                served_on_custom_provider_endpoint: false,
            },
        }
    }

    /// Persistence/load backend family for this input type.
    #[must_use]
    pub const fn persistence(self) -> InputPersistence { self.capabilities().persistence }
}

/// Storage/loading backend family an [`InputType`] maps onto.
///
/// Multiple input variants collapse onto the same persistence family (for
/// example every media-server variant shares the same on-disk format), so
/// persist/load routing can match on this instead of re-listing variants.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InputPersistence {
    M3u,
    Xtream,
    Library,
    MediaServer,
    Stalker,
}

/// Categorical capabilities of an [`InputType`], declared once in
/// [`InputType::capabilities`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct InputCapabilities {
    /// Storage/loading backend family.
    pub persistence: InputPersistence,
    /// Whether generic stream probing must open a provider connection.
    pub requires_provider_connection_for_probe: bool,
    /// Whether the custom-provider HTTP endpoint can serve this input.
    pub served_on_custom_provider_endpoint: bool,
}

#[derive(
    Debug,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    EnumIter,
    PartialEq,
    Eq,
    Default,
    Display,
    EnumString,
    AsRefStr,
)]
#[strum(serialize_all = "UPPERCASE")]
#[serde(rename_all = "UPPERCASE")]
pub enum InputFetchMethod {
    #[default]
    GET,
    POST,
}

impl InputFetchMethod {
    pub fn is_default(value: &InputFetchMethod) -> bool { matches!(value, Self::GET) }
}
