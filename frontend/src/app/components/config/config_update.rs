use crate::app::components::config::config_page::ConfigForm;
use shared::model::{
    ConfigDto, ContentSecurityPolicyConfigDto, HdHomeRunConfigDto, LibraryConfigDto, LibraryMetadataConfigDto,
    LibraryPlaylistConfigDto, WebAuthConfigDto, WebUiConfigDto,
};

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

fn is_hdhomerun_toggle_only_update(cfg: &HdHomeRunConfigDto) -> bool {
    cfg.devices.is_empty() && !cfg.auth && !cfg.ssdp_discovery && !cfg.proprietary_discovery
}

fn is_library_toggle_only_update(cfg: &LibraryConfigDto) -> bool {
    cfg.scan_directories.is_empty()
        && cfg.supported_extensions.is_empty()
        && cfg.metadata == LibraryMetadataConfigDto::default()
        && cfg.playlist == LibraryPlaylistConfigDto::default()
}

fn update_hdhomerun_field(config: &mut ConfigDto, mut hdhr_cfg: HdHomeRunConfigDto) {
    if let Some(existing) = config.hdhomerun.as_mut() {
        if is_hdhomerun_toggle_only_update(&hdhr_cfg) {
            // Setup/edit toggles can emit sparse/default payloads; keep existing details.
            existing.enabled = hdhr_cfg.enabled;
            return;
        }
    }

    if hdhr_cfg.is_empty() {
        config.hdhomerun = None;
    } else {
        hdhr_cfg.clean();
        config.hdhomerun = Some(hdhr_cfg);
    }
}

fn update_library_field(config: &mut ConfigDto, mut library_cfg: LibraryConfigDto) {
    if let Some(existing) = config.library.as_mut() {
        if is_library_toggle_only_update(&library_cfg) {
            // Setup/edit toggles can emit sparse/default payloads; keep existing details.
            existing.enabled = library_cfg.enabled;
            return;
        }
    }

    if library_cfg.is_empty() {
        config.library = None;
    } else {
        library_cfg.clean();
        config.library = Some(library_cfg);
    }
}

fn is_webui_toggle_only_update(cfg: &WebUiConfigDto) -> bool {
    cfg.path.as_deref().is_none_or(|path| path.trim().is_empty())
        && cfg.player_server.as_deref().is_none_or(|player_server| player_server.trim().is_empty())
        && cfg.kick_secs == WebUiConfigDto::default().kick_secs
        && cfg.auth.as_ref().is_none_or(WebAuthConfigDto::is_empty)
        && cfg.content_security_policy.as_ref().is_none_or(ContentSecurityPolicyConfigDto::is_empty)
}

fn update_webui_field(config: &mut ConfigDto, mut web_ui_cfg: WebUiConfigDto) {
    if let Some(existing) = config.web_ui.as_mut() {
        if is_webui_toggle_only_update(&web_ui_cfg) {
            // Toggle-only form updates must not drop existing nested WebUI payload.
            existing.enabled = web_ui_cfg.enabled;
            existing.user_ui_enabled = web_ui_cfg.user_ui_enabled;
            return;
        }
    }

    if web_ui_cfg.is_empty() {
        config.web_ui = None;
    } else {
        web_ui_cfg.clean();
        config.web_ui = Some(web_ui_cfg);
    }
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
            }
            ConfigForm::Video(_, mut video_cfg) => set_config_field!(config, video_cfg, video),
            ConfigForm::Messaging(_, mut messaging_cfg) => set_config_field!(config, messaging_cfg, messaging),
            ConfigForm::WebUi(_, web_ui_cfg) => update_webui_field(config, web_ui_cfg),
            ConfigForm::ReverseProxy(_, mut reverse_proxy_cfg) => {
                set_config_field!(config, reverse_proxy_cfg, reverse_proxy)
            }
            ConfigForm::HdHomerun(_, hdhr_cfg) => update_hdhomerun_field(config, hdhr_cfg),
            ConfigForm::Proxy(_, mut proxy_cfg) => set_config_field!(config, proxy_cfg, proxy),
            ConfigForm::IpCheck(_, mut ipcheck_cfg) => set_config_field!(config, ipcheck_cfg, ipcheck),
            ConfigForm::Library(_, library_cfg) => update_library_field(config, library_cfg),
            ConfigForm::Panel(_, _) => {}
            ConfigForm::ApiProxy(_, _) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::update_config;
    use crate::app::components::config::config_page::ConfigForm;
    use shared::model::{
        ConfigDto, ContentSecurityPolicyConfigDto, HdHomeRunConfigDto, HdHomeRunDeviceConfigDto, LibraryConfigDto,
        LibraryScanDirectoryDto, ProxyConfigDto, WebAuthConfigDto, WebUiConfigDto,
    };

    #[test]
    fn update_config_keeps_library_payload_on_empty_toggle() {
        let mut config = ConfigDto::default();
        config.library = Some(LibraryConfigDto {
            enabled: true,
            scan_directories: vec![LibraryScanDirectoryDto {
                enabled: true,
                path: "/media".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        });

        update_config(&mut config, vec![ConfigForm::Library(true, LibraryConfigDto::default())]);

        let library = config.library.expect("library config should be preserved");
        assert!(!library.enabled);
        assert_eq!(library.scan_directories.len(), 1);
        assert_eq!(library.scan_directories[0].path, "/media");
    }

    #[test]
    fn update_config_keeps_hdhomerun_payload_on_empty_toggle() {
        let mut config = ConfigDto::default();
        config.hdhomerun = Some(HdHomeRunConfigDto {
            enabled: true,
            devices: vec![HdHomeRunDeviceConfigDto { name: "living_room".to_string(), ..Default::default() }],
            ..Default::default()
        });

        update_config(&mut config, vec![ConfigForm::HdHomerun(true, HdHomeRunConfigDto::default())]);

        let hdhr = config.hdhomerun.expect("hdhomerun config should be preserved");
        assert!(!hdhr.enabled);
        assert_eq!(hdhr.devices.len(), 1);
        assert_eq!(hdhr.devices[0].name, "living_room");
    }

    #[test]
    fn update_config_keeps_proxy_empty_as_none() {
        let mut config = ConfigDto::default();
        config.proxy = Some(ProxyConfigDto { url: "http://proxy.local".to_string(), username: None, password: None });

        update_config(&mut config, vec![ConfigForm::Proxy(true, ProxyConfigDto::default())]);

        assert!(config.proxy.is_none());
    }

    #[test]
    fn update_config_keeps_webui_payload_on_toggle_only() {
        let mut config = ConfigDto::default();
        config.web_ui = Some(WebUiConfigDto {
            enabled: true,
            user_ui_enabled: true,
            path: Some("/dashboard".to_string()),
            player_server: Some("http://player.local".to_string()),
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "tuliprox".to_string(),
                secret: "top-secret".to_string(),
                ..Default::default()
            }),
            content_security_policy: Some(ContentSecurityPolicyConfigDto {
                enabled: true,
                custom_attributes: Some(vec!["default-src 'self'".to_string()]),
            }),
            ..Default::default()
        });

        update_config(
            &mut config,
            vec![ConfigForm::WebUi(
                true,
                WebUiConfigDto {
                    enabled: false,
                    user_ui_enabled: false,
                    auth: Some(WebAuthConfigDto::default()),
                    content_security_policy: Some(ContentSecurityPolicyConfigDto::default()),
                    ..Default::default()
                },
            )],
        );

        let web_ui = config.web_ui.expect("webui config should be preserved");
        assert!(!web_ui.enabled);
        assert!(!web_ui.user_ui_enabled);
        assert_eq!(web_ui.path.as_deref(), Some("/dashboard"));
        assert_eq!(web_ui.player_server.as_deref(), Some("http://player.local"));
        assert_eq!(web_ui.auth.as_ref().map(|auth| auth.secret.as_str()), Some("top-secret"));
    }
}
