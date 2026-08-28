use crate::app::components::config::{ConfigForm, ConfigFormSlots, ConfigPage};
use shared::{
    model::{SourcesConfigDto, TargetUserDto},
    utils::Internable,
};
use std::{fmt, sync::Arc};
use yew::UseStateHandle;

const WELCOME: &str = "welcome";
const API: &str = "api";
const WEB_UI: &str = "web_ui";
const MAIN: &str = "main";
const LOG: &str = "log";
const MESSAGING: &str = "messaging";
const REVERSE_PROXY: &str = "reverse_proxy";
const PROXY: &str = "proxy";
const IPCHECK: &str = "ipcheck";
const VIDEO: &str = "video";
const METADATA_UPDATE: &str = "metadata_update";
const HDHOMERUN: &str = "hdhomerun";
const LIBRARY: &str = "library";
const SOURCES: &str = "sources";
const API_USERS: &str = "api_users";
const SCHEDULES: &str = "schedules";
const FINISH: &str = "finish";

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
    MetadataUpdate,
    HdHomerun,
    Library,
    Sources,
    ApiUsers,
    Schedules,
    Finish,
}

impl SetupStep {
    #[cfg(test)]
    pub const ALL_VARIANTS: [Self; 17] = [
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
        Self::MetadataUpdate,
        Self::HdHomerun,
        Self::Library,
        Self::Sources,
        Self::ApiUsers,
        Self::Schedules,
        Self::Finish,
    ];

    pub const ORDER: [Self; 17] = [
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
        Self::MetadataUpdate,
        Self::HdHomerun,
        Self::Library,
        Self::Sources,
        Self::ApiUsers,
        Self::Schedules,
        Self::Finish,
    ];

    pub fn all() -> &'static [Self] {
        &Self::ORDER
    }

    pub fn index(self) -> usize {
        Self::all().iter().position(|step| *step == self).expect("SetupStep::ORDER must include all variants")
    }

    pub fn position(self) -> usize {
        self.index() + 1
    }

    pub fn total() -> usize {
        Self::ORDER.len()
    }

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

    pub const fn title_key(self) -> &'static str {
        match self {
            Self::Welcome => "SETUP.LABEL.WELCOME",
            Self::Api => "SETUP.LABEL.API",
            Self::WebUi => "SETUP.LABEL.WEB_UI",
            Self::Main => "SETUP.LABEL.MAIN",
            Self::Log => "SETUP.LABEL.LOG",
            Self::Messaging => "SETUP.LABEL.MESSAGING",
            Self::ReverseProxy => "SETUP.LABEL.REVERSE_PROXY",
            Self::Proxy => "SETUP.LABEL.PROXY",
            Self::IpCheck => "SETUP.LABEL.IP_CHECK",
            Self::Video => "SETUP.LABEL.VIDEO",
            Self::MetadataUpdate => "SETUP.LABEL.METADATA_UPDATE",
            Self::HdHomerun => "SETUP.LABEL.HDHOMERUN",
            Self::Library => "SETUP.LABEL.LIBRARY",
            Self::Sources => "SETUP.LABEL.SOURCES",
            Self::ApiUsers => "SETUP.LABEL.API_USERS",
            Self::Schedules => "SETUP.LABEL.SCHEDULES",
            Self::Finish => "SETUP.LABEL.FINISH",
        }
    }

    pub const fn description_key(self) -> &'static str {
        match self {
            Self::Welcome => "SETUP.DESC.WELCOME",
            Self::Api => "SETUP.DESC.API",
            Self::WebUi => "SETUP.DESC.WEB_UI",
            Self::Main => "SETUP.DESC.MAIN",
            Self::Log => "SETUP.DESC.LOG",
            Self::Messaging => "SETUP.DESC.MESSAGING",
            Self::ReverseProxy => "SETUP.DESC.REVERSE_PROXY",
            Self::Proxy => "SETUP.DESC.PROXY",
            Self::IpCheck => "SETUP.DESC.IP_CHECK",
            Self::Video => "SETUP.DESC.VIDEO",
            Self::MetadataUpdate => "SETUP.DESC.METADATA_UPDATE",
            Self::HdHomerun => "SETUP.DESC.HDHOMERUN",
            Self::Library => "SETUP.DESC.LIBRARY",
            Self::Sources => "SETUP.DESC.SOURCES",
            Self::ApiUsers => "SETUP.DESC.API_USERS",
            Self::Schedules => "SETUP.DESC.SCHEDULES",
            Self::Finish => "SETUP.DESC.FINISH",
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
            Self::MetadataUpdate => Some(ConfigPage::MetadataUpdate),
            Self::HdHomerun => Some(ConfigPage::HdHomerun),
            Self::Library => Some(ConfigPage::Library),
            Self::Schedules => Some(ConfigPage::Schedules),
            Self::Welcome | Self::Sources | Self::ApiUsers | Self::Finish => None,
        }
    }
}

impl SetupStep {
    pub fn as_str(&self) -> &'static str {
        match self {
            SetupStep::Welcome => WELCOME,
            SetupStep::Api => API,
            SetupStep::WebUi => WEB_UI,
            SetupStep::Main => MAIN,
            SetupStep::Log => LOG,
            SetupStep::Messaging => MESSAGING,
            SetupStep::ReverseProxy => REVERSE_PROXY,
            SetupStep::Proxy => PROXY,
            SetupStep::IpCheck => IPCHECK,
            SetupStep::Video => VIDEO,
            SetupStep::MetadataUpdate => METADATA_UPDATE,
            SetupStep::HdHomerun => HDHOMERUN,
            SetupStep::Library => LIBRARY,
            SetupStep::Sources => SOURCES,
            SetupStep::ApiUsers => API_USERS,
            SetupStep::Schedules => SCHEDULES,
            SetupStep::Finish => FINISH,
        }
    }
}

impl fmt::Display for SetupStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.as_str();
        write!(f, "{value}")
    }
}

impl Internable for SetupStep {
    fn intern(self) -> Arc<str> {
        self.as_str().intern()
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
pub struct SetupConfigFormState {
    pub slots: ConfigFormSlots,
}

impl SetupConfigFormState {
    pub fn update_form(&mut self, form: ConfigForm) {
        self.slots.update_form(form);
    }

    pub fn collect_modified_forms(&self) -> Vec<ConfigForm> {
        self.slots.collect_modified_forms()
    }
}

#[derive(Clone)]
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

impl PartialEq for SetupContext {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::SetupStep;
    use std::collections::BTreeSet;

    #[test]
    fn setup_step_order_matches_all_variants_exactly_once() {
        assert_eq!(SetupStep::ORDER.len(), SetupStep::ALL_VARIANTS.len());

        let order_set: BTreeSet<SetupStep> = SetupStep::ORDER.into_iter().collect();
        let all_variants_set: BTreeSet<SetupStep> = SetupStep::ALL_VARIANTS.into_iter().collect();

        assert_eq!(order_set, all_variants_set);
        assert_eq!(order_set.len(), SetupStep::ORDER.len());
    }
}
