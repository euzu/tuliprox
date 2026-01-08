use shared::model::ConfigDto;
use crate::app::components::config::config_page::ConfigForm;

macro_rules! set_config_field {
    ($main_config:expr, $config:expr, $field:ident) => {
        if $config.is_empty() {
            $main_config.$field = None;
        } else {
            $config.clean();
            $main_config.$field = Some($config);
        }
    };
}

pub fn update_config(config: &mut ConfigDto, forms: Vec<ConfigForm>) {
    for form in forms {
        match form {
            ConfigForm::Main(_, main_cfg) => config.update_from_main_config(&main_cfg),
            ConfigForm::Api(_, api_cfg) => config.api = api_cfg,
            ConfigForm::Log(_, mut log_cfg) => set_config_field!(config, log_cfg, log),
            ConfigForm::Schedules(_, schedules_cfg) => {
                if schedules_cfg.schedules.is_none() || schedules_cfg.schedules.as_ref().is_some_and(|s| s.is_empty()) {
                    config.schedules = None;
                } else {
                    config.schedules = schedules_cfg.schedules.clone();
                }
            },
            ConfigForm::Video(_, mut video_cfg) =>  set_config_field!(config, video_cfg, video),
            ConfigForm::Messaging(_, mut messaging_cfg) => set_config_field!(config, messaging_cfg, messaging),
            ConfigForm::WebUi(_, mut web_ui_cfg) => set_config_field!(config, web_ui_cfg, web_ui),
            ConfigForm::ReverseProxy(_, mut reverse_proxy_cfg) => set_config_field!(config, reverse_proxy_cfg, reverse_proxy),
            ConfigForm::HdHomerun(_, mut hdhr_cfg) => set_config_field!(config, hdhr_cfg, hdhomerun),
            ConfigForm::Proxy(_, mut proxy_cfg) => set_config_field!(config, proxy_cfg, proxy),
            ConfigForm::IpCheck(_, mut ipcheck_cfg) => set_config_field!(config, ipcheck_cfg, ipcheck),
            ConfigForm::Library(_, mut library_cfg) => set_config_field!(config, library_cfg, library),
            ConfigForm::Panel(_, _) => {}
        }
    }
}
