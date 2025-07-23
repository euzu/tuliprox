use crate::utils::{default_as_true};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogConfigDto {
    #[serde(default = "default_as_true")]
    pub sanitize_sensitive_info: bool,
    #[serde(default)]
    pub log_active_user: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
}
