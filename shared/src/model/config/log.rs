use crate::{
    defaults::{default_as_true, is_default_runtime_config_report_format, is_false, is_true},
    utils::{is_blank_optional_str, is_blank_optional_string},
};
use strum_macros::{AsRefStr, Display, EnumIter, EnumString};

#[derive(
    Debug,
    Clone,
    Copy,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    Default,
    EnumIter,
    EnumString,
    AsRefStr,
    Display,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum RuntimeConfigReportFormat {
    #[default]
    Yaml,
    Json,
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
    fn runtime_config_report_defaults_to_disabled_yaml() {
        let cfg = LogConfigDto::default();
        assert!(!cfg.runtime_config_report_enabled);
        assert_eq!(cfg.runtime_config_report_format, super::RuntimeConfigReportFormat::Yaml);
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
        assert!(!encoded.contains("\"runtime_config_report_format\":\"yaml\""));
    }
}
