use shared::model::LogConfigDto;
use shared::utils::default_as_true;
use crate::model::macros;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct LogLevelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LogConfig>,
}

macros::from_impl!(LogConfig);
impl From<&LogConfigDto> for LogConfig {
    fn from(dto: &LogConfigDto) -> Self {
        Self {
            sanitize_sensitive_info: dto.sanitize_sensitive_info,
            log_active_user: dto.log_active_user,
            log_level: dto.log_level.clone(),
        }
    }
}
impl From<&LogConfig> for LogConfigDto {
    fn from(instance: &LogConfig) -> Self {
        Self {
            sanitize_sensitive_info: instance.sanitize_sensitive_info,
            log_active_user: instance.log_active_user,
            log_level: instance.log_level.clone(),
        }
    }
}