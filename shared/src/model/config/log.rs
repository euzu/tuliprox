use crate::utils::{default_as_true, is_blank_optional_str, is_blank_optional_string, is_false, is_true};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub sanitize_sensitive_info: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub log_active_user: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub log_level: Option<String>,
}

impl Default for LogConfigDto {
    fn default() -> Self {
        LogConfigDto { sanitize_sensitive_info: default_as_true(), log_active_user: false, log_level: None }
    }
}

impl LogConfigDto {
    pub fn is_empty(&self) -> bool {
        self.sanitize_sensitive_info && !self.log_active_user && is_blank_optional_str(self.log_level.as_deref())
    }

    pub fn clean(&mut self) {
        if is_blank_optional_str(self.log_level.as_deref()) {
            self.log_level = None;
        }
    }
}
