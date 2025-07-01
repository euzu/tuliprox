use shared::model::{EpgConfigDto, EpgSourceDto};
use crate::model::{macros, EpgSmartMatchConfig};

#[derive(Debug, Clone)]
pub struct EpgSource {
    pub url: String,
    pub priority: i16,
    pub logo_override: bool,
}

macros::from_impl!(EpgSource);
impl From<&EpgSourceDto> for EpgSource {
    fn from(dto: &EpgSourceDto) -> Self {
        Self {
            url: dto.url.to_string(),
            priority: dto.priority,
            logo_override: dto.logo_override,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EpgConfig {
    pub sources: Vec<EpgSource>,
    pub smart_match: Option<EpgSmartMatchConfig>,
}

macros::from_impl!(EpgConfig);
impl From<&EpgConfigDto> for EpgConfig {
    fn from(dto: &EpgConfigDto) -> Self {
        Self {
            sources: dto.t_sources.iter().map(Into::into).collect(),
            smart_match: dto.smart_match.as_ref().map(Into::into),
        }
    }
}
