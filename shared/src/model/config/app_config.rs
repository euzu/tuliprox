use crate::model::{ApiProxyConfigDto, ConfigDto, MappingsDto, SourcesConfigDto};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppConfigDto {
    pub config: ConfigDto,
    pub sources: SourcesConfigDto,
    pub mappings: Option<MappingsDto>,
    pub api_proxy: Option<ApiProxyConfigDto>,
}