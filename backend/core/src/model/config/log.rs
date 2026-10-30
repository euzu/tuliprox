use crate::model::macros;
use shared::model::{LogConfigDto, LogLevel, RuntimeConfigReportFormat, SyslogConfigDto, SyslogFacility, SyslogFormat};

#[derive(Debug, Clone, PartialEq)]
pub struct SyslogConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub format: SyslogFormat,
    pub facility: SyslogFacility,
    pub app_name: String,
    pub hostname: Option<String>,
    pub log_level: Option<LogLevel>,
    pub queue_capacity: u32,
    pub max_message_len: u16,
}

impl Default for SyslogConfig {
    fn default() -> Self { Self::from(&SyslogConfigDto::default()) }
}

macros::from_impl!(SyslogConfig);

impl From<&SyslogConfigDto> for SyslogConfig {
    fn from(dto: &SyslogConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            host: dto.host.clone(),
            port: dto.port,
            format: dto.format,
            facility: dto.facility,
            app_name: dto.app_name.clone(),
            hostname: dto.hostname.clone(),
            log_level: dto.log_level,
            queue_capacity: dto.queue_capacity,
            max_message_len: dto.max_message_len,
        }
    }
}

impl From<&SyslogConfig> for SyslogConfigDto {
    fn from(instance: &SyslogConfig) -> Self {
        Self {
            enabled: instance.enabled,
            host: instance.host.clone(),
            port: instance.port,
            format: instance.format,
            facility: instance.facility,
            app_name: instance.app_name.clone(),
            hostname: instance.hostname.clone(),
            log_level: instance.log_level,
            queue_capacity: instance.queue_capacity,
            max_message_len: instance.max_message_len,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LogConfig {
    pub sanitize_sensitive_info: bool,
    pub log_active_user: bool,
    pub log_level: Option<String>,
    pub runtime_config_report_enabled: bool,
    pub runtime_config_report_format: RuntimeConfigReportFormat,
    pub syslog: SyslogConfig,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct LogLevelConfig {
    #[serde(default)]
    pub log: Option<LogConfigDto>,
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
            syslog: SyslogConfig::from(&dto.syslog),
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
            syslog: SyslogConfigDto::from(&instance.syslog),
        }
    }
}
