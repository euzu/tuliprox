use serde::{Deserialize, Serialize};
use shared::model::{ApiProxyConfigDto, ConfigApiDto, ConfigRenameDto, ConfigSortDto, ConfigTargetOptions, InputType, IpCheckConfigDto, LogConfigDto, MessagingConfigDto, ProcessingOrder, ProxyConfigDto, ReverseProxyConfigDto, ScheduleConfigDto, TargetOutputDto, VideoConfigDto, WebUiConfigDto};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ServerInputConfig {
    pub id: u16,
    pub input_type: InputType,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub persist: Option<String>,
    pub name: String,
    pub enabled: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ServerTargetConfig {
    pub id: u16,
    pub enabled: bool,
    pub name: String,
    pub options: Option<ConfigTargetOptions>,
    pub sort: Option<ConfigSortDto>,
    pub filter: String,
    #[serde(alias = "type")]
    pub output: Vec<TargetOutputDto>,
    pub rename: Option<Vec<ConfigRenameDto>>,
    pub mapping: Option<Vec<String>>,
    pub processing_order: ProcessingOrder,
    pub watch: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ServerSourceConfig {
    pub inputs: Vec<ServerInputConfig>,
    pub targets: Vec<ServerTargetConfig>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ServerConfig {
    pub api: ConfigApiDto,
    pub threads: u8,
    pub working_dir: String,
    pub backup_dir: Option<String>,
    pub user_config_dir: Option<String>,
    pub schedules: Option<Vec<ScheduleConfigDto>>,
    pub reverse_proxy: Option<ReverseProxyConfigDto>,
    pub sources: Vec<ServerSourceConfig>,
    pub messaging: Option<MessagingConfigDto>,
    pub video: Option<VideoConfigDto>,
    pub api_proxy: Option<ApiProxyConfigDto>,
    pub log: Option<LogConfigDto>,
    pub update_on_boot: bool,
    pub web_ui: Option<WebUiConfigDto>,
    pub proxy: Option<ProxyConfigDto>,
    pub ipcheck: Option<IpCheckConfigDto>,
}

