use crate::error::TuliproxError;
use enum_iterator::Sequence;
use std::{fmt::Display, str::FromStr};

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, Sequence, PartialEq, Eq, Hash, Default)]
pub enum StrmExportStyle {
    #[serde(rename = "kodi")]
    #[default]
    Kodi,
    #[serde(rename = "emby")]
    Emby,
    #[serde(rename = "jellyfin")]
    Jellyfin,
}

impl StrmExportStyle {
    const KODI: &'static str = "Kodi";
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
                Self::Emby => Self::EMBY,
                Self::Jellyfin => Self::JELLYFIN,
            }
        )
    }
}

impl FromStr for StrmExportStyle {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            Self::KODI => Ok(Self::Kodi),
            Self::EMBY => Ok(Self::Emby),
            Self::JELLYFIN => Ok(Self::Jellyfin),
            _ => Err(TuliproxError::Config(format!("Unknown StrmExportStyle: {s}"))),
        }
    }
}
