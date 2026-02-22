use shared::{
    error::{info_err_res, TuliproxError},
    model::{
        ApiProxyConfigDto, ConfigApiDto, HdHomeRunConfigDto, IpCheckConfigDto, LibraryConfigDto, LogConfigDto,
        MainConfigDto, MessagingConfigDto, ProxyConfigDto, ReverseProxyConfigDto, SchedulesConfigDto, SourcesConfigDto,
        VideoConfigDto, WebUiConfigDto,
    },
};
use std::{fmt, str::FromStr};

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
    ApiProxy(bool, ApiProxyConfigDto),
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
        matches!(
            self,
            ConfigForm::Main(true, _)
                | ConfigForm::Api(true, _)
                | ConfigForm::ApiProxy(true, _)
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
                | ConfigForm::Library(true, _)
        )
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
pub struct ConfigFormSlots {
    pub main: Option<ConfigForm>,
    pub api: Option<ConfigForm>,
    pub api_proxy: Option<ConfigForm>,
    pub log: Option<ConfigForm>,
    pub schedules: Option<ConfigForm>,
    pub video: Option<ConfigForm>,
    pub messaging: Option<ConfigForm>,
    pub web_ui: Option<ConfigForm>,
    pub reverse_proxy: Option<ConfigForm>,
    pub hd_homerun: Option<ConfigForm>,
    pub proxy: Option<ConfigForm>,
    pub ipcheck: Option<ConfigForm>,
    pub panel: Option<ConfigForm>,
    pub library: Option<ConfigForm>,
}

impl ConfigFormSlots {
    fn set_form_slot(slot: &mut Option<ConfigForm>, form: ConfigForm) {
        if slot.as_ref().is_some_and(|existing| existing.is_modified() && !form.is_modified()) {
            return;
        }
        *slot = Some(form);
    }

    pub fn update_form(&mut self, form: ConfigForm) {
        match form {
            ConfigForm::Main(_, _) => Self::set_form_slot(&mut self.main, form),
            ConfigForm::Api(_, _) => Self::set_form_slot(&mut self.api, form),
            ConfigForm::ApiProxy(_, _) => Self::set_form_slot(&mut self.api_proxy, form),
            ConfigForm::Log(_, _) => Self::set_form_slot(&mut self.log, form),
            ConfigForm::Schedules(_, _) => Self::set_form_slot(&mut self.schedules, form),
            ConfigForm::Video(_, _) => Self::set_form_slot(&mut self.video, form),
            ConfigForm::Messaging(_, _) => Self::set_form_slot(&mut self.messaging, form),
            ConfigForm::WebUi(_, _) => Self::set_form_slot(&mut self.web_ui, form),
            ConfigForm::ReverseProxy(_, _) => Self::set_form_slot(&mut self.reverse_proxy, form),
            ConfigForm::HdHomerun(_, _) => Self::set_form_slot(&mut self.hd_homerun, form),
            ConfigForm::Proxy(_, _) => Self::set_form_slot(&mut self.proxy, form),
            ConfigForm::IpCheck(_, _) => Self::set_form_slot(&mut self.ipcheck, form),
            ConfigForm::Panel(_, _) => Self::set_form_slot(&mut self.panel, form),
            ConfigForm::Library(_, _) => Self::set_form_slot(&mut self.library, form),
        }
    }

    pub fn all_slots(&self) -> [&Option<ConfigForm>; 14] {
        [
            &self.main,
            &self.api,
            &self.api_proxy,
            &self.log,
            &self.schedules,
            &self.video,
            &self.messaging,
            &self.web_ui,
            &self.reverse_proxy,
            &self.hd_homerun,
            &self.proxy,
            &self.ipcheck,
            &self.panel,
            &self.library,
        ]
    }

    pub fn collect_modified_forms(&self) -> Vec<ConfigForm> {
        self.all_slots().into_iter().flatten().filter(|form| form.is_modified()).cloned().collect()
    }
}
