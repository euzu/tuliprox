use crate::model::macros;
use shared::{
    defaults::default_as_true,
    model::{LogConfigDto, RuntimeConfigReportFormat},
};

// We need serde for these structs to read them during
// start from the yaml file without reading the whole config.
//
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[serde(default = "default_as_true")]
    pub sanitize_sensitive_info: bool,
    #[serde(default)]
    pub log_active_user: bool,
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub runtime_config_report_enabled: bool,
    #[serde(default)]
    pub runtime_config_report_format: RuntimeConfigReportFormat,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct LogLevelConfig {
    #[serde(default)]
    pub log: Option<LogConfig>,
}

macros::from_impl!(LogConfig);
impl From<&LogConfigDto> for LogConfig {
    fn from(dto: &LogConfigDto) -> Self {
        Self {
            sanitize_sensitive_info: dto.sanitize_sensitive_info,
            log_active_user: dto.log_active_user,
            log_level: dto.log_level.clone(),
            runtime_config_report_enabled: dto.runtime_config_report_enabled,
            runtime_config_report_format: dto.runtime_config_report_format,
        }
    }
}
impl From<&LogConfig> for LogConfigDto {
    fn from(instance: &LogConfig) -> Self {
        Self {
            sanitize_sensitive_info: instance.sanitize_sensitive_info,
            log_active_user: instance.log_active_user,
            log_level: instance.log_level.clone(),
            runtime_config_report_enabled: instance.runtime_config_report_enabled,
            runtime_config_report_format: instance.runtime_config_report_format,
        }
    }
}
