use shared::error::{TuliproxError, info_err_res};
use shared::model::{ConfigApiDto, HdHomeRunConfigDto, IpCheckConfigDto, LibraryConfigDto, LogConfigDto, MainConfigDto, MessagingConfigDto, ProxyConfigDto, ReverseProxyConfigDto, SchedulesConfigDto, SourcesConfigDto, VideoConfigDto, WebUiConfigDto};
use std::fmt;
use std::str::FromStr;

pub const LABEL_MAIN_CONFIG: &str = "LABEL.MAIN_CONFIG";
pub const LABEL_API_CONFIG: &str = "LABEL.API_CONFIG";
pub const LABEL_LOG_CONFIG: &str = "LABEL.LOG_CONFIG";
pub const LABEL_SCHEDULES_CONFIG: &str = "LABEL.SCHEDULES_CONFIG";
pub const LABEL_MESSAGING_CONFIG: &str = "LABEL.MESSAGING_CONFIG";
pub const LABEL_WEB_UI_CONFIG: &str = "LABEL.WEB_UI_CONFIG";
pub const LABEL_REVERSE_PROXY_CONFIG: &str = "LABEL.REVERSE_PROXY_CONFIG";
pub const LABEL_HDHOMERUN_CONFIG: &str = "LABEL.HDHOMERUN_CONFIG";
pub const LABEL_PROXY_CONFIG: &str = "LABEL.PROXY_CONFIG";
pub const LABEL_IP_CHECK_CONFIG: &str = "LABEL.IP_CHECK_CONFIG";
pub const LABEL_VIDEO_CONFIG: &str = "LABEL.VIDEO_CONFIG";
pub const LABEL_PANEL_CONFIG: &str = "LABEL.PANEL_CONFIG";
pub const LABEL_LIBRARY_CONFIG: &str = "LABEL.LIBRARY_CONFIG";

const MAIN_PAGE: &str = "main";
const API_PAGE: &str = "api";
const LOG_PAGE: &str = "log";
const SCHEDULES_PAGE: &str = "schedules";
const MESSAGING_PAGE: &str = "messaging";
const WEBUI_PAGE: &str = "webui";
const REVERSE_PROXY_PAGE: &str = "reverse_proxy";
const HDHOMERUN_PAGE: &str = "hdhomerun";
const PROXY_PAGE: &str = "proxy";
const IPCHECK_PAGE: &str = "ipcheck";
const VIDEO_PAGE: &str = "video";
const PANEL_PAGE: &str = "panel";
const LIBRARY_PAGE: &str = "library";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum ConfigPage {
    Main,
    Api,
    Log,
    Schedules,
    Video,
    Messaging,
    WebUi,
    ReverseProxy,
    HdHomerun,
    Proxy,
    IpCheck,
    Panel,
    Library,
}

impl FromStr for ConfigPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            MAIN_PAGE => Ok(ConfigPage::Main),
            API_PAGE => Ok(ConfigPage::Api),
            LOG_PAGE => Ok(ConfigPage::Log),
            SCHEDULES_PAGE => Ok(ConfigPage::Schedules),
            VIDEO_PAGE => Ok(ConfigPage::Video),
            MESSAGING_PAGE => Ok(ConfigPage::Messaging),
            WEBUI_PAGE => Ok(ConfigPage::WebUi),
            REVERSE_PROXY_PAGE => Ok(ConfigPage::ReverseProxy),
            HDHOMERUN_PAGE => Ok(ConfigPage::HdHomerun),
            PROXY_PAGE => Ok(ConfigPage::Proxy),
            IPCHECK_PAGE => Ok(ConfigPage::IpCheck),
            PANEL_PAGE => Ok(ConfigPage::Panel),
            LIBRARY_PAGE => Ok(ConfigPage::Library),
            _ => info_err_res!("Unknown config page: {s}"),
        }
    }
}

impl fmt::Display for ConfigPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ConfigPage::Main => MAIN_PAGE,
            ConfigPage::Api => API_PAGE,
            ConfigPage::Log => LOG_PAGE,
            ConfigPage::Schedules => SCHEDULES_PAGE,
            ConfigPage::Video => VIDEO_PAGE,
            ConfigPage::Messaging => MESSAGING_PAGE,
            ConfigPage::WebUi => WEBUI_PAGE,
            ConfigPage::ReverseProxy => REVERSE_PROXY_PAGE,
            ConfigPage::HdHomerun => HDHOMERUN_PAGE,
            ConfigPage::Proxy => PROXY_PAGE,
            ConfigPage::IpCheck => IPCHECK_PAGE,
            ConfigPage::Panel => PANEL_PAGE,
            ConfigPage::Library => LIBRARY_PAGE,
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigForm {
    Main(bool, MainConfigDto),
    Api(bool, ConfigApiDto),
    Log(bool, LogConfigDto),
    Schedules(bool, SchedulesConfigDto),
    Video(bool, VideoConfigDto),
    Messaging(bool, MessagingConfigDto),
    WebUi(bool, WebUiConfigDto),
    ReverseProxy(bool, ReverseProxyConfigDto),
    HdHomerun(bool, HdHomeRunConfigDto),
    Proxy(bool, ProxyConfigDto),
    IpCheck(bool, IpCheckConfigDto),
    Panel(bool, SourcesConfigDto),
    Library(bool, LibraryConfigDto),
}

impl ConfigForm {
    pub(crate) fn is_modified(&self) -> bool {
        matches!(self, ConfigForm::Main(true, _)
              | ConfigForm::Api(true, _)
              | ConfigForm::Log(true, _)
              | ConfigForm::Schedules(true, _)
              | ConfigForm::Video(true, _)
              | ConfigForm::Messaging(true, _)
              | ConfigForm::WebUi(true, _)
              | ConfigForm::ReverseProxy(true, _)
              | ConfigForm::HdHomerun(true, _)
              | ConfigForm::Proxy(true, _)
              | ConfigForm::IpCheck(true, _)
              | ConfigForm::Panel(true, _)
              | ConfigForm::Library(true, _))
    }
}
