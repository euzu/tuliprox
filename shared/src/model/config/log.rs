use crate::{
    error::TuliproxError,
    utils::{
        default_as_true, is_blank_optional_str, is_blank_optional_string, is_default_runtime_config_report_format,
        is_false, is_true,
    },
};
use enum_iterator::Sequence;
use std::{fmt::Display, str::FromStr};

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default, Sequence)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeConfigReportFormat {
    #[default]
    Yaml,
    Json,
}

impl RuntimeConfigReportFormat {
    const JSON: &'static str = "json";
    const YAML: &'static str = "yaml";
}

impl Display for RuntimeConfigReportFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = match self {
            RuntimeConfigReportFormat::Yaml => RuntimeConfigReportFormat::YAML.to_string(),
            RuntimeConfigReportFormat::Json => RuntimeConfigReportFormat::JSON.to_string(),
        };
        write!(f, "{str}")
    }
}

impl FromStr for RuntimeConfigReportFormat {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            RuntimeConfigReportFormat::YAML => Ok(RuntimeConfigReportFormat::Yaml),
            RuntimeConfigReportFormat::JSON => Ok(RuntimeConfigReportFormat::Json),
            _ => Err(TuliproxError::Config(format!("Invalid Runtime config report format {s}"))),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub sanitize_sensitive_info: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub log_active_user: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub log_level: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub runtime_config_report_enabled: bool,
    #[serde(default, skip_serializing_if = "is_default_runtime_config_report_format")]
    pub runtime_config_report_format: RuntimeConfigReportFormat,
}

impl Default for LogConfigDto {
    fn default() -> Self {
        LogConfigDto {
            sanitize_sensitive_info: default_as_true(),
            log_active_user: false,
            log_level: None,
            runtime_config_report_enabled: false,
            runtime_config_report_format: RuntimeConfigReportFormat::default(),
        }
    }
}

impl LogConfigDto {
    pub fn is_empty(&self) -> bool {
        self.sanitize_sensitive_info
            && !self.log_active_user
            && is_blank_optional_str(self.log_level.as_deref())
            && !self.runtime_config_report_enabled
            && is_default_runtime_config_report_format(&self.runtime_config_report_format)
    }

    pub fn clean(&mut self) {
        if is_blank_optional_str(self.log_level.as_deref()) {
            self.log_level = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogConfigDto;

    #[test]
    fn runtime_config_report_defaults_to_disabled_json() {
        let cfg = LogConfigDto::default();
        assert!(!cfg.runtime_config_report_enabled);
        assert_eq!(cfg.runtime_config_report_format, super::RuntimeConfigReportFormat::Json);
    }

    #[test]
    fn runtime_config_report_yaml_round_trips() {
        let json = r#"{
            "runtime_config_report_enabled": true,
            "runtime_config_report_format": "yaml"
        }"#;
        let cfg: LogConfigDto = serde_json::from_str(json).expect("deserialize");
        assert!(cfg.runtime_config_report_enabled);
        assert_eq!(cfg.runtime_config_report_format, super::RuntimeConfigReportFormat::Yaml);

        let encoded = serde_json::to_string(&cfg).expect("serialize");
        assert!(encoded.contains("\"runtime_config_report_enabled\":true"));
        assert!(encoded.contains("\"runtime_config_report_format\":\"yaml\""));
    }
}
