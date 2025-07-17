use serde::{Deserialize, Serialize};


#[derive(PartialEq, Eq, Clone, Serialize, Deserialize, Debug)]
pub struct ApiConfig {
    #[serde(alias = "apiUrl")]
    pub api_url: String,
    #[serde(alias = "authUrl")]
    pub auth_url: String,
}

#[derive(PartialEq, Eq, Clone, Serialize, Deserialize, Debug)]
pub struct WebConfig {
    #[serde(alias = "tabTitle")]
    pub tab_title: Option<String>,
    #[serde(alias = "appTitle")]
    pub app_title: Option<String>,
    #[serde(alias = "appLogo")]
    pub app_logo: Option<String>,
    pub api: ApiConfig,
    #[serde(default)]
    pub discord: String,
    #[serde(default)]
    pub github: String,
    #[serde(default)]
    pub documentation: String,
    #[serde(alias = "wsUrl")]
    pub ws_url: String,
    #[serde(alias = "protocolVersion")]
    pub protocol_version: u8,

}
