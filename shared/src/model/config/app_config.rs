use crate::model::{ApiProxyConfigDto, ConfigDto, MappingsDto, SourcesConfigDto, TemplateDefinitionDto};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppConfigDto {
    pub config: ConfigDto,
    pub sources: SourcesConfigDto,
    pub mappings: Option<MappingsDto>,
    pub templates: Option<TemplateDefinitionDto>,
    pub api_proxy: Option<ApiProxyConfigDto>,
}

impl AppConfigDto {
    pub fn is_stream_history_enabled(&self) -> bool { self.config.is_stream_history_enabled() }

    pub fn is_qos_aggregation_enabled(&self) -> bool { self.config.is_qos_aggregation_enabled() }
}
