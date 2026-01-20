use crate::utils::{default_as_true, is_true};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiQueryParamDto {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiQueryParametersDto {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_info: Vec<PanelApiQueryParamDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_new: Vec<PanelApiQueryParamDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_renew: Vec<PanelApiQueryParamDto>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PanelApiConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub query_parameter: PanelApiQueryParametersDto,
}

impl Default for PanelApiConfigDto {
    fn default() -> Self {
        Self {
            enabled: true,
            url: String::new(),
            api_key: None,
            query_parameter: PanelApiQueryParametersDto::default(),
        }
    }
}
