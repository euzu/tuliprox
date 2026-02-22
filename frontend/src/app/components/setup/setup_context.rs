use crate::app::components::config::{ConfigForm, ConfigPage};
use shared::model::{SourcesConfigDto, TargetUserDto};
use std::fmt;
use yew::UseStateHandle;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SetupStep {
    Welcome,
    Api,
    WebUi,
    Main,
    Log,
    Messaging,
    ReverseProxy,
    Proxy,
    IpCheck,
    Video,
    HdHomerun,
    Library,
    Sources,
    ApiUsers,
    Schedules,
    Finish,
}

impl SetupStep {
    pub const ORDER: [Self; 16] = [
        Self::Welcome,
        Self::Api,
        Self::WebUi,
        Self::Main,
        Self::Log,
        Self::Messaging,
        Self::ReverseProxy,
        Self::Proxy,
        Self::IpCheck,
        Self::Video,
        Self::HdHomerun,
        Self::Library,
        Self::Sources,
        Self::ApiUsers,
        Self::Schedules,
        Self::Finish,
    ];

    pub fn all() -> &'static [Self] { &Self::ORDER }

    pub fn index(self) -> usize {
        match self {
            Self::Welcome => 0,
            Self::Api => 1,
            Self::WebUi => 2,
            Self::Main => 3,
            Self::Log => 4,
            Self::Messaging => 5,
            Self::ReverseProxy => 6,
            Self::Proxy => 7,
            Self::IpCheck => 8,
            Self::Video => 9,
            Self::HdHomerun => 10,
            Self::Library => 11,
            Self::Sources => 12,
            Self::ApiUsers => 13,
            Self::Schedules => 14,
            Self::Finish => 15,
        }
    }

    pub fn position(self) -> usize { self.index() + 1 }

    pub fn total() -> usize { Self::ORDER.len() }

    pub fn next(self) -> Option<Self> {
        let index = self.index();
        if index + 1 < Self::ORDER.len() {
            Some(Self::ORDER[index + 1])
        } else {
            None
        }
    }

    pub fn prev(self) -> Option<Self> {
        let index = self.index();
        if index == 0 {
            None
        } else {
            Some(Self::ORDER[index - 1])
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Api => "Api",
            Self::WebUi => "WebUi",
            Self::Main => "Main",
            Self::Log => "Log",
            Self::Messaging => "Messaging",
            Self::ReverseProxy => "ReverseProxy",
            Self::Proxy => "Proxy",
            Self::IpCheck => "IpCheck",
            Self::Video => "Video",
            Self::HdHomerun => "HdHomerun",
            Self::Library => "Library",
            Self::Sources => "Sources",
            Self::ApiUsers => "ApiUsers",
            Self::Schedules => "Schedules",
            Self::Finish => "Finish",
        }
    }

    pub fn config_page(self) -> Option<ConfigPage> {
        match self {
            Self::Api => Some(ConfigPage::Api),
            Self::WebUi => Some(ConfigPage::WebUi),
            Self::Main => Some(ConfigPage::Main),
            Self::Log => Some(ConfigPage::Log),
            Self::Messaging => Some(ConfigPage::Messaging),
            Self::ReverseProxy => Some(ConfigPage::ReverseProxy),
            Self::Proxy => Some(ConfigPage::Proxy),
            Self::IpCheck => Some(ConfigPage::IpCheck),
            Self::Video => Some(ConfigPage::Video),
            Self::HdHomerun => Some(ConfigPage::HdHomerun),
            Self::Library => Some(ConfigPage::Library),
            Self::Schedules => Some(ConfigPage::Schedules),
            Self::Welcome | Self::Sources | Self::ApiUsers | Self::Finish => None,
        }
    }
}

impl fmt::Display for SetupStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            SetupStep::Welcome => "welcome",
            SetupStep::Api => "api",
            SetupStep::WebUi => "web_ui",
            SetupStep::Main => "main",
            SetupStep::Log => "log",
            SetupStep::Messaging => "messaging",
            SetupStep::ReverseProxy => "reverse_proxy",
            SetupStep::Proxy => "proxy",
            SetupStep::IpCheck => "ipcheck",
            SetupStep::Video => "video",
            SetupStep::HdHomerun => "hdhomerun",
            SetupStep::Library => "library",
            SetupStep::Sources => "sources",
            SetupStep::ApiUsers => "api_users",
            SetupStep::Schedules => "schedules",
            SetupStep::Finish => "finish",
        };
        write!(f, "{value}")
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
pub struct SetupConfigFormState {
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

impl SetupConfigFormState {
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

    pub fn collect_modified_forms(&self) -> Vec<ConfigForm> {
        let mut modified = Vec::new();
        let forms = [
            self.main.as_ref(),
            self.api.as_ref(),
            self.api_proxy.as_ref(),
            self.log.as_ref(),
            self.schedules.as_ref(),
            self.video.as_ref(),
            self.messaging.as_ref(),
            self.web_ui.as_ref(),
            self.reverse_proxy.as_ref(),
            self.hd_homerun.as_ref(),
            self.proxy.as_ref(),
            self.ipcheck.as_ref(),
            self.panel.as_ref(),
            self.library.as_ref(),
        ];
        for form in forms.into_iter().flatten() {
            if form.is_modified() {
                modified.push(form.clone());
            }
        }
        modified
    }
}

#[derive(Clone, PartialEq)]
pub struct SetupContext {
    pub active_step: UseStateHandle<SetupStep>,
    pub max_unlocked_step: UseStateHandle<SetupStep>,
    pub setup_username: UseStateHandle<String>,
    pub setup_password: UseStateHandle<String>,
    pub setup_password_repeat: UseStateHandle<String>,
    pub config_forms: UseStateHandle<SetupConfigFormState>,
    pub sources: UseStateHandle<SourcesConfigDto>,
    pub api_users: UseStateHandle<Vec<TargetUserDto>>,
    pub is_submitting: UseStateHandle<bool>,
    pub is_completed: UseStateHandle<bool>,
    pub submit_error: UseStateHandle<Option<String>>,
}
