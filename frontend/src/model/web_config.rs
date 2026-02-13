use serde::{Deserialize, Serialize};

pub fn default_documentation_url() -> String {
    String::from("https://euzu.github.io/tuliprox-docs/")
}

#[derive(PartialEq, Eq, Clone, Serialize, Deserialize, Debug, Default)]
pub struct ApiConfig {
    #[serde(alias = "apiUrl")]
    pub api_url: String,
    #[serde(alias = "authUrl")]
    pub auth_url: String,
}

#[derive(PartialEq, Eq, Clone, Serialize, Deserialize, Debug, Default)]
pub struct WebConfig {
    #[serde(alias = "webPath")]
    pub web_path: Option<String>,
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
    #[serde(default = "default_documentation_url")]
    pub documentation: String,
    #[serde(alias = "wsUrl")]
    pub ws_url: String,
    #[serde(alias = "protocolVersion")]
    pub protocol_version: u8,
}
