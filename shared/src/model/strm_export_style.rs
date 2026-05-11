use strum_macros::{Display, EnumIter, EnumString};

#[derive(
    Debug,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Hash,
    Default,
    EnumIter,
    Display,
    EnumString,
)]
#[strum(serialize_all = "PascalCase")]
#[serde(rename_all = "lowercase")]
pub enum StrmExportStyle {
    #[default]
    Kodi,
    Emby,
    Jellyfin,
}
