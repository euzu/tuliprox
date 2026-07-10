use crate::model::{macros, EpgSmartMatchConfig};
use shared::model::{
    EpgConfigDto, EpgSourceDto, EpgSourceTypeDto, IcsDummyConfigDto, IcsEpgSourceConfigDto,
    IcsEventMappingDto,
};
use shared::utils::Internable;
use std::sync::Arc;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum EpgSourceType {
    Xmltv,
    Ics,
}

impl From<EpgSourceTypeDto> for EpgSourceType {
    fn from(value: EpgSourceTypeDto) -> Self {
        match value {
            EpgSourceTypeDto::Xmltv => Self::Xmltv,
            EpgSourceTypeDto::Ics => Self::Ics,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EpgSource {
    pub source_type: EpgSourceType,
    pub url: String,
    pub priority: i16,
    pub logo_override: bool,
    pub channel_id: Option<Arc<str>>,
    pub channel_title: Option<Arc<str>>,
    pub match_names: Vec<Arc<str>>,
    pub ics: Option<IcsEpgSourceConfig>,
}

macros::from_impl!(EpgSource);
impl From<&EpgSourceDto> for EpgSource {
    fn from(dto: &EpgSourceDto) -> Self {
        Self {
            source_type: EpgSourceType::from(dto.source_type),
            url: dto.url.clone(),
            priority: dto.priority,
            logo_override: dto.logo_override,
            channel_id: dto.channel_id.as_deref().map(Internable::intern),
            channel_title: dto.channel_title.as_deref().map(Internable::intern),
            match_names: dto.match_names.iter().map(|name| name.as_str().intern()).collect(),
            ics: dto.ics.as_ref().map(IcsEpgSourceConfig::from),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IcsEpgSourceConfig {
    pub timezone: String,
    pub event: IcsEventMapping,
    pub dummy: IcsDummyConfig,
    pub include_cancelled: bool,
    pub max_events: usize,
    pub max_download_bytes: u64,
    pub max_decompressed_bytes: usize,
}

impl Default for IcsEpgSourceConfig {
    fn default() -> Self { Self::from(&IcsEpgSourceConfigDto::default()) }
}

impl From<&IcsEpgSourceConfigDto> for IcsEpgSourceConfig {
    fn from(dto: &IcsEpgSourceConfigDto) -> Self {
        Self {
            timezone: dto.timezone.clone(),
            event: IcsEventMapping::from(&dto.event),
            dummy: IcsDummyConfig::from(&dto.dummy),
            include_cancelled: dto.include_cancelled,
            max_events: dto.max_events,
            max_download_bytes: dto.max_download_bytes,
            max_decompressed_bytes: dto.max_decompressed_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IcsEventMapping {
    pub title: String,
    pub description: String,
    pub include_location: bool,
    pub include_categories: bool,
}

impl From<&IcsEventMappingDto> for IcsEventMapping {
    fn from(dto: &IcsEventMappingDto) -> Self {
        Self {
            title: dto.title.clone(),
            description: dto.description.clone(),
            include_location: dto.include_location,
            include_categories: dto.include_categories,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IcsDummyConfig {
    pub enabled: bool,
    pub title: String,
    pub description: String,
    pub days_past: u16,
    pub days_future: u16,
    pub block_hours: u8,
    pub min_gap_minutes: u16,
}

impl From<&IcsDummyConfigDto> for IcsDummyConfig {
    fn from(dto: &IcsDummyConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            title: dto.title.clone(),
            description: dto.description.clone(),
            days_past: dto.days_past,
            days_future: dto.days_future,
            block_hours: dto.block_hours,
            min_gap_minutes: dto.min_gap_minutes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IcsDummyPolicy {
    pub timezone: String,
    pub config: IcsDummyConfig,
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
            sources: dto.t_sources.iter().map(EpgSource::from).collect(),
            smart_match: dto.smart_match.as_ref().map(EpgSmartMatchConfig::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{EpgSourceTypeDto, IcsEpgSourceConfigDto};

    #[test]
    fn epg_source_dto_type_xmltv_maps_to_runtime_xmltv() {
        let dto = EpgSourceDto {
            source_type: EpgSourceTypeDto::Xmltv,
            url: "https://example.com/xmltv.xml".to_string(),
            ..EpgSourceDto::default()
        };

        let source = EpgSource::from(&dto);

        assert_eq!(source.source_type, EpgSourceType::Xmltv);
        assert_eq!(source.url, "https://example.com/xmltv.xml");
    }

    #[test]
    fn epg_source_dto_type_ics_maps_channel_metadata() {
        let dto = EpgSourceDto {
            source_type: EpgSourceTypeDto::Ics,
            url: "https://example.com/f1.ics".to_string(),
            channel_id: Some("f1.calendar".to_string()),
            channel_title: Some("Formula 1".to_string()),
            match_names: vec!["F1".to_string(), "Formel 1".to_string()],
            ics: Some(IcsEpgSourceConfigDto::default()),
            ..EpgSourceDto::default()
        };

        let source = EpgSource::from(&dto);

        assert_eq!(source.source_type, EpgSourceType::Ics);
        assert_eq!(source.channel_id.as_deref(), Some("f1.calendar"));
        assert_eq!(source.channel_title.as_deref(), Some("Formula 1"));
        assert_eq!(
            source.match_names.iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
            vec!["F1", "Formel 1"]
        );
        assert_eq!(source.ics.as_ref().map(|config| config.max_events), Some(50_000));
    }
}
