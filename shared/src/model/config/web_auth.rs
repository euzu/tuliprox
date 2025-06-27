use crate::utils::{default_as_true};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebAuthConfigDto {
    #[serde(default = "default_as_true")]
    pub enabled: bool,
    pub issuer: String,
    pub secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub userfile: Option<String>,
}
