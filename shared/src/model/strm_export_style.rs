use enum_iterator::Sequence;
use std::fmt::Display;
use std::str::FromStr;

#[derive(
    Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Hash, Default,
)]
pub enum StrmExportStyle {
    #[serde(rename = "kodi")]
    #[default]
    Kodi,
    #[serde(rename = "plex")]
    Plex,
    #[serde(rename = "emby")]
    Emby,
    #[serde(rename = "jellyfin")]
    Jellyfin,
}

impl StrmExportStyle {
    const KODI: &'static str = "Kodi";
    const PLEX: &'static str = "Plex";
    const EMBY: &'static str = "Emby";
    const JELLYFIN: &'static str = "Jellyfin";
}

impl Display for StrmExportStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::Kodi => Self::KODI,
                Self::Plex => Self::PLEX,
                Self::Emby => Self::EMBY,
                Self::Jellyfin => Self::JELLYFIN,
            }
        )
    }
}

impl FromStr for StrmExportStyle {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            Self::KODI => Ok(Self::Kodi),
            Self::PLEX => Ok(Self::Plex),
            Self::EMBY => Ok(Self::Emby),
            Self::JELLYFIN => Ok(Self::Jellyfin),
            _ => Err(format!("Unknown StrmExportStyle: {}", s)),
        }
    }
}
