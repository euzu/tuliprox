use crate::utils::{
    default_as_true, default_token_ttl_mins, default_user_file_path, is_blank_or_default_user_file_path,
    is_default_token_ttl_mins, is_true,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebAuthConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub issuer: String,
    pub secret: String,
    #[serde(default = "default_token_ttl_mins", skip_serializing_if = "is_default_token_ttl_mins")]
    pub token_ttl_mins: u32,
    #[serde(default = "default_user_file_path", skip_serializing_if = "is_blank_or_default_user_file_path")]
    pub userfile: Option<String>,
}

impl Default for WebAuthConfigDto {
    fn default() -> Self {
        Self {
            enabled: default_as_true(),
            issuer: String::new(),
            secret: String::new(),
            token_ttl_mins: default_token_ttl_mins(),
            userfile: None,
        }
    }
}

impl WebAuthConfigDto {
    pub fn is_empty(&self) -> bool {
        let empty = WebAuthConfigDto::default();
        self.enabled == empty.enabled
            && self.token_ttl_mins == empty.token_ttl_mins
            && self.issuer.trim().is_empty()
            && self.secret.trim().is_empty()
            && is_blank_or_default_user_file_path(&self.userfile)
    }
}
