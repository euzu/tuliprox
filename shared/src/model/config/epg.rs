use crate::{defaults::is_false, model::EpgSmartMatchConfigDto, utils::is_blank_optional_string};

const AUTO_URL: &str = "auto";

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum EpgSourceTypeDto {
    #[default]
    Xmltv,
    Ics,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EpgSourceDto {
    #[serde(default, rename = "type")]
    pub source_type: EpgSourceTypeDto,
    pub url: String,
    #[serde(default)]
    pub priority: i16,
    #[serde(default, skip_serializing_if = "is_false")]
    pub logo_override: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub channel_title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ics: Option<IcsEpgSourceConfigDto>,
}

impl Default for EpgSourceDto {
    fn default() -> Self {
        Self {
            source_type: EpgSourceTypeDto::Xmltv,
            url: String::new(),
            priority: 0,
            logo_override: false,
            channel_id: None,
            channel_title: None,
            match_names: Vec::new(),
            ics: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct EpgConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<EpgSourceDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_match: Option<EpgSmartMatchConfigDto>,
    #[serde(skip)]
    pub t_sources: Vec<EpgSourceDto>,
}

#[cfg(test)]
mod tests;

mod ics;
mod prepare;
pub use ics::{IcsDummyConfigDto, IcsEpgSourceConfigDto, IcsEventMappingDto};
