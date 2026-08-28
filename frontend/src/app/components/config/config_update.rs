use crate::app::components::config::config_page::ConfigForm;
use shared::{
    defaults::is_default_supported_library_extensions,
    error::TuliproxError,
    model::{
        ConfigDto, HdHomeRunConfigDto, LibraryConfigDto, LibraryMetadataConfigDto, LibraryPlaylistConfigDto,
        ThumbnailConfigDto, WebUiConfigDto,
    },
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

fn is_library_toggle_only_update(cfg: &LibraryConfigDto) -> bool {
    cfg.scan_directories.is_empty()
        && (cfg.supported_extensions.is_empty() || is_default_supported_library_extensions(&cfg.supported_extensions))
        && cfg.metadata == LibraryMetadataConfigDto::default()
        && cfg.playlist == LibraryPlaylistConfigDto::default()
        && cfg.thumbnails == ThumbnailConfigDto::default()
}

fn update_hdhomerun_field(config: &mut ConfigDto, mut hdhr_cfg: HdHomeRunConfigDto) {
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

fn update_webui_field(config: &mut ConfigDto, web_ui_cfg: WebUiConfigDto, modified: bool) {
    // If there's no existing web_ui config: clean and store the form (empty form -> None).
    if config.web_ui.is_none() {
        if web_ui_cfg.is_empty() {
            return;
        }
        let mut cfg = web_ui_cfg;
        let kick_secs = cfg.kick_secs;
        cfg.clean();
        cfg.kick_secs = kick_secs;
        config.web_ui = Some(cfg);
        return;
    }

    if !modified && web_ui_cfg.is_empty() {
        return;
    }

    if !modified {
        let existing = config.web_ui.as_mut().unwrap();
        existing.enabled = web_ui_cfg.enabled;
        existing.user_ui_enabled = web_ui_cfg.user_ui_enabled;
        existing.combine_views_stats_streams = web_ui_cfg.combine_views_stats_streams;
        existing.landing_page = web_ui_cfg.landing_page;

        if let Some(auth) = web_ui_cfg.auth.filter(|auth| !auth.is_empty()) {
            existing.auth = Some(auth);
        }
        if let Some(csp) = web_ui_cfg.content_security_policy.filter(|csp| !csp.is_empty()) {
            existing.content_security_policy = Some(csp);
        }
        if web_ui_cfg.path.as_deref().is_some_and(|path| {
            let trimmed = path.trim();
            !trimmed.is_empty() && !trimmed.chars().all(|c| c == '/')
        }) {
            existing.path = web_ui_cfg.path;
        }
        if web_ui_cfg.player_server.as_deref().is_some_and(|player_server| !player_server.trim().is_empty()) {
            existing.player_server = web_ui_cfg.player_server;
        }
        if let Some(stream_info) = web_ui_cfg.stream_info.filter(|stream_info| !stream_info.is_empty()) {
            existing.stream_info = Some(stream_info);
        }
        let kick_secs = existing.kick_secs;
        existing.clean();
        existing.kick_secs = kick_secs;
        return;
    }

    if modified && web_ui_cfg.is_empty() {
        config.web_ui = None;
    } else {
        let mut cfg = web_ui_cfg;
        let kick_secs = cfg.kick_secs;
        cfg.clean();
        cfg.kick_secs = kick_secs;
        config.web_ui = Some(cfg);
    }
}

pub fn update_config(config: &mut ConfigDto, forms: Vec<ConfigForm>) {
    for form in forms {
        match form {
            ConfigForm::Main(_, main_cfg) => config.update_from_main_config(&main_cfg),
            ConfigForm::Api(_, api_cfg) => config.api = api_cfg,
            ConfigForm::Log(_, mut log_cfg) => set_config_field!(config, log_cfg, log),
            ConfigForm::Schedules(_, schedules_cfg) => {
                if schedules_cfg.schedules.is_none()
                    || schedules_cfg.schedules.as_ref().is_some_and(std::vec::Vec::is_empty)
                {
                    config.schedules = None;
                } else {
                    config.schedules = schedules_cfg.schedules.clone();
                }
            }
            ConfigForm::Recording(_, mut recording_cfg) => set_config_field!(config, recording_cfg, recording),
            ConfigForm::MetadataUpdate(_, mut metadata_update_cfg) => {
                set_config_field!(config, metadata_update_cfg, metadata_update);
            }
            ConfigForm::Messaging(_, mut messaging_cfg) => set_config_field!(config, messaging_cfg, messaging),
            ConfigForm::WebUi(modified, web_ui_cfg) => update_webui_field(config, web_ui_cfg, modified),
            ConfigForm::ReverseProxy(_, mut reverse_proxy_cfg) => {
                set_config_field!(config, reverse_proxy_cfg, reverse_proxy);
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

pub fn apply_and_prepare_config(config: &mut ConfigDto, forms: Vec<ConfigForm>) -> Result<(), TuliproxError> {
    update_config(config, forms);
    config.prepare(false)
}

#[cfg(test)]
mod tests {
    use super::{apply_and_prepare_config, update_config};
    use crate::app::components::config::config_page::ConfigForm;
    use shared::model::{
        view_type::ViewType, ConfigDto, ContentSecurityPolicyConfigDto, DiskAlertConfigDto, HdHomeRunConfigDto,
        HdHomeRunDeviceConfigDto, LibraryConfigDto, LibraryScanDirectoryDto, MessagingConfigDto,
        MetadataUpdateConfigDto, MsgKind, ProxyConfigDto, RecordingConfigDto, RecordingDiskConfigDto,
        RecordingNotificationConfigDto, RecordingQuotaConfigDto, RecordingRetentionConfigDto, StreamInfoConfigDto,
        WebAuthConfigDto, WebUiConfigDto,
    };

    #[test]
    fn update_config_keeps_library_payload_on_empty_toggle() {
        let mut config = ConfigDto {
            library: Some(LibraryConfigDto {
                enabled: true,
                scan_directories: vec![LibraryScanDirectoryDto {
                    enabled: true,
                    path: "/media".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(&mut config, vec![ConfigForm::Library(true, LibraryConfigDto::default())]);

        let library = config.library.expect("library config should be preserved");
        assert!(!library.enabled);
        assert!(!library.thumbnails.enabled);
        assert_eq!(library.scan_directories.len(), 1);
        assert_eq!(library.scan_directories[0].path, "/media");
    }

    #[test]
    fn update_config_hdhomerun_empty_form_clears_existing_payload() {
        let mut config = ConfigDto {
            hdhomerun: Some(HdHomeRunConfigDto {
                enabled: true,
                devices: vec![HdHomeRunDeviceConfigDto { name: "living_room".to_string(), ..Default::default() }],
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(&mut config, vec![ConfigForm::HdHomerun(true, HdHomeRunConfigDto::default())]);

        assert!(config.hdhomerun.is_none(), "empty forms must clear existing HDHomeRun payload");
    }

    #[test]
    fn update_config_hdhomerun_allows_removing_all_devices() {
        let mut config = ConfigDto {
            hdhomerun: Some(HdHomeRunConfigDto {
                enabled: true,
                devices: vec![HdHomeRunDeviceConfigDto { name: "living_room".to_string(), ..Default::default() }],
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(
            &mut config,
            vec![ConfigForm::HdHomerun(
                true,
                HdHomeRunConfigDto { enabled: true, devices: Vec::new(), ..Default::default() },
            )],
        );

        let hdhr = config.hdhomerun.expect("hdhomerun config should still be present");
        assert!(hdhr.enabled);
        assert!(hdhr.devices.is_empty(), "devices should be removed instead of restored");
    }

    #[test]
    fn update_config_hdhomerun_allows_removing_all_devices_while_disabled() {
        let mut config = ConfigDto {
            hdhomerun: Some(HdHomeRunConfigDto {
                enabled: false,
                devices: vec![HdHomeRunDeviceConfigDto { name: "living_room".to_string(), ..Default::default() }],
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(&mut config, vec![ConfigForm::HdHomerun(true, HdHomeRunConfigDto::default())]);

        assert!(config.hdhomerun.is_none(), "disabled empty forms should clear the HDHomeRun config");
    }

    #[test]
    fn update_config_keeps_proxy_empty_as_none() {
        let mut config = ConfigDto {
            proxy: Some(ProxyConfigDto { url: "http://proxy.local".to_string(), username: None, password: None }),
            ..ConfigDto::default()
        };

        update_config(&mut config, vec![ConfigForm::Proxy(true, ProxyConfigDto::default())]);

        assert!(config.proxy.is_none());
    }

    #[test]
    fn update_config_keeps_webui_payload_on_toggle_only() {
        let mut config = ConfigDto {
            web_ui: Some(WebUiConfigDto {
                enabled: true,
                user_ui_enabled: true,
                path: Some("/dashboard".to_string()),
                player_server: Some("http://player.local".to_string()),
                auth: Some(WebAuthConfigDto {
                    enabled: true,
                    issuer: "tuliprox".to_string(),
                    secret: "top-secret".to_string(),
                    groupfile: Some("groups.txt".to_string()),
                    ..Default::default()
                }),
                content_security_policy: Some(ContentSecurityPolicyConfigDto {
                    enabled: true,
                    custom_attributes: Some(vec!["default-src 'self'".to_string()]),
                }),
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(
            &mut config,
            vec![ConfigForm::WebUi(
                false, // <-- all nested fields are default/empty, preserve existing config
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
        assert_eq!(web_ui.auth.as_ref().and_then(|auth| auth.groupfile.as_deref()), Some("groups.txt"));
    }

    #[test]
    fn update_config_metadata_update_empty_form_clears_config() {
        let mut config = ConfigDto::default();

        update_config(&mut config, vec![ConfigForm::MetadataUpdate(true, MetadataUpdateConfigDto::default())]);

        assert!(config.metadata_update.is_none());
    }

    #[test]
    fn update_config_metadata_update_applies_and_cleans_payload() {
        let mut config = ConfigDto::default();
        let mut metadata_cfg = MetadataUpdateConfigDto::default();
        metadata_cfg.ffprobe.enabled = true;
        metadata_cfg.ffprobe.timeout = Some(60);

        assert!(!metadata_cfg.is_empty());
        metadata_cfg.clean();
        assert_eq!(metadata_cfg.ffprobe.timeout, Some(60));

        update_config(&mut config, vec![ConfigForm::MetadataUpdate(true, metadata_cfg)]);

        let stored = config.metadata_update.as_ref().expect("metadata_update config should be set");
        assert!(stored.ffprobe.enabled);
        assert_eq!(stored.ffprobe.timeout, Some(60));
    }

    #[test]
    fn update_config_webui_explicit_stream_info_clear() {
        // User explicitly clears all hide_* flags via the form (stream_info=None).
        // modified=true signals explicit edit; existing stream_info must become None.
        let mut config = ConfigDto {
            web_ui: Some(WebUiConfigDto {
                enabled: true,
                stream_info: Some(StreamInfoConfigDto { hide_ip: true, ..Default::default() }),
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(
            &mut config,
            vec![ConfigForm::WebUi(
                true, // modified=true: explicit edit
                WebUiConfigDto {
                    enabled: false,
                    stream_info: None, // user cleared all flags
                    ..Default::default()
                },
            )],
        );

        let web_ui = config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert!(web_ui.stream_info.is_none(), "stream_info should be explicitly cleared to None");
    }

    #[test]
    fn update_config_webui_explicit_landing_page_reset_to_default() {
        // User explicitly resets landing_page to default while also changing scalars.
        // The reset landing_page value must be applied literally, not preserved.
        let mut config = ConfigDto {
            web_ui: Some(WebUiConfigDto {
                enabled: true,
                landing_page: Some(ViewType::Streams), // existing is non-default
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(
            &mut config,
            vec![ConfigForm::WebUi(
                true, // modified=true: explicit edit
                WebUiConfigDto {
                    enabled: false,
                    landing_page: None, // reset to last-page default
                    ..Default::default()
                },
            )],
        );

        let web_ui = config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert_eq!(web_ui.landing_page, None, "landing_page reset to default must be applied");
    }

    #[test]
    fn update_config_webui_toggle_only_preserves_stream_info() {
        // Toggle-only sparse update (modified=false): preserve existing stream_info.
        let mut config = ConfigDto {
            web_ui: Some(WebUiConfigDto {
                enabled: true,
                stream_info: Some(StreamInfoConfigDto { hide_ip: true, hide_country: true, ..Default::default() }),
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(
            &mut config,
            vec![ConfigForm::WebUi(
                false, // modified=false: toggle-only, preserve nested config
                WebUiConfigDto {
                    enabled: false,
                    stream_info: None, // form has None but this is "untouched", not "cleared"
                    ..Default::default()
                },
            )],
        );

        let web_ui = config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        let stream_info = web_ui.stream_info.expect("stream_info should be preserved on toggle-only");
        assert!(stream_info.hide_ip);
        assert!(stream_info.hide_country);
    }

    #[test]
    fn update_config_webui_toggle_only_preserves_landing_page() {
        // Toggle-only sparse update (modified=false): landing_page in form matches existing
        // (form syncs landing_page from existing on open). Landing_page should be preserved.
        let mut config = ConfigDto {
            web_ui: Some(WebUiConfigDto {
                enabled: true,
                landing_page: Some(ViewType::Streams), // existing is non-default
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(
            &mut config,
            vec![ConfigForm::WebUi(
                false, // modified=false: toggle-only, landing_page unchanged in form
                WebUiConfigDto {
                    enabled: false,
                    landing_page: Some(ViewType::Streams), // form syncs from existing on open
                    ..Default::default()
                },
            )],
        );

        let web_ui = config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert_eq!(web_ui.landing_page, Some(ViewType::Streams), "landing_page should be preserved on toggle-only");
    }

    #[test]
    fn update_config_webui_toggle_only_with_nested_field_change_does_selective_update() {
        // User changed auth issuer (nested field change), landing_page stays at synced value.
        // modified=true means this is a real form edit and nested values must be applied literally.
        let mut config = ConfigDto {
            web_ui: Some(WebUiConfigDto {
                enabled: true,
                landing_page: Some(ViewType::Streams),
                auth: Some(WebAuthConfigDto { issuer: "test".to_string(), ..Default::default() }),
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(
            &mut config,
            vec![ConfigForm::WebUi(
                true, // explicit edit: nested values must be applied
                WebUiConfigDto {
                    enabled: false,
                    landing_page: Some(ViewType::Streams), // form syncs from existing
                    auth: Some(WebAuthConfigDto { issuer: "other".to_string(), ..Default::default() }),
                    ..Default::default()
                },
            )],
        );

        // Selective update: scalars and landing_page applied, auth preserved (nested change).
        let web_ui = config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert_eq!(web_ui.landing_page, Some(ViewType::Streams));
        assert_eq!(web_ui.auth.as_ref().map(|a| a.issuer.as_str()), Some("other"));
    }

    #[test]
    fn update_config_webui_explicit_clear_auth_path_player_server_and_reset_kick_secs() {
        let mut config = ConfigDto {
            web_ui: Some(WebUiConfigDto {
                enabled: true,
                path: Some("/dashboard".to_string()),
                player_server: Some("http://player.local".to_string()),
                kick_secs: 321,
                auth: Some(WebAuthConfigDto {
                    enabled: true,
                    issuer: "issuer".to_string(),
                    secret: "top-secret".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..ConfigDto::default()
        };

        update_config(
            &mut config,
            vec![ConfigForm::WebUi(
                true,
                WebUiConfigDto {
                    enabled: false,
                    path: None,
                    player_server: None,
                    kick_secs: WebUiConfigDto::default().kick_secs,
                    auth: Some(WebAuthConfigDto::default()),
                    ..Default::default()
                },
            )],
        );

        let web_ui = config.web_ui.expect("webui config should be present");
        assert!(!web_ui.enabled);
        assert!(web_ui.path.is_none());
        assert!(web_ui.player_server.is_none());
        assert_eq!(web_ui.kick_secs, WebUiConfigDto::default().kick_secs);
        assert!(web_ui.auth.is_none());
    }

    #[test]
    fn update_config_clears_default_disk_alert_block_via_messaging_form() {
        // Web UI save path: enable DiskAlert in notify_on with default
        // thresholds. The `disk_alert` block must survive `set_config_field!`
        // (is_empty → clean) so the runtime gates (`notify_on` contains
        // DiskAlert AND `disk_alert` is Some) fire.
        let mut config = ConfigDto::default();

        update_config(
            &mut config,
            vec![ConfigForm::Messaging(
                true,
                MessagingConfigDto {
                    notify_on: vec![MsgKind::DiskAlert],
                    disk_alert: Some(DiskAlertConfigDto::default()),
                    ..Default::default()
                },
            )],
        );

        let messaging = config.messaging.as_ref().expect("messaging config should be set");
        assert_eq!(messaging.disk_alert, None);
    }

    #[test]
    fn update_config_preserves_non_default_disk_alert_block_via_messaging_form() {
        // Non-default thresholds must round-trip verbatim through
        // `set_config_field!`, matching the default-threshold case above.
        let mut config = ConfigDto::default();
        let disk_alert = DiskAlertConfigDto { warn_percent: 70.0, critical_percent: 90.0, repeat_interval_secs: 600 };

        update_config(
            &mut config,
            vec![ConfigForm::Messaging(
                true,
                MessagingConfigDto {
                    notify_on: vec![MsgKind::DiskAlert],
                    disk_alert: Some(disk_alert.clone()),
                    ..Default::default()
                },
            )],
        );

        let messaging = config.messaging.as_ref().expect("messaging config should be set");
        assert_eq!(messaging.disk_alert, Some(disk_alert));
    }

    #[test]
    fn update_config_round_trips_explicit_recording_and_cleans_empty_nested_blocks() {
        let quota = RecordingQuotaConfigDto { shared_bytes: Some(1024), ..RecordingQuotaConfigDto::default() };
        let recording = RecordingConfigDto {
            retention: Some(RecordingRetentionConfigDto::default()),
            disk: Some(RecordingDiskConfigDto::default()),
            quota: Some(quota.clone()),
            notifications: Some(RecordingNotificationConfigDto::default()),
            ..RecordingConfigDto::default()
        };
        let mut config = ConfigDto::default();

        update_config(&mut config, vec![ConfigForm::Recording(true, recording)]);

        let serialized = serde_json::to_string(&config);
        assert!(serialized.is_ok(), "config should serialize");
        let Ok(serialized) = serialized else { return };
        let config: Result<ConfigDto, _> = serde_json::from_str(&serialized);
        assert!(config.is_ok(), "config should deserialize");
        let Ok(config) = config else { return };
        let recording = config.recording.as_ref();
        assert!(recording.is_some(), "explicit recording block should round-trip");
        let Some(recording) = recording else { return };
        assert!(recording.retention.is_none());
        assert!(recording.disk.is_none());
        assert_eq!(recording.quota, Some(quota));
        assert!(recording.notifications.is_none());
    }

    #[test]
    fn update_config_round_trips_explicit_empty_recording() {
        let mut config = ConfigDto::default();

        update_config(&mut config, vec![ConfigForm::Recording(true, RecordingConfigDto::default())]);

        let serialized = serde_json::to_string(&config);
        assert!(serialized.is_ok(), "config should serialize");
        let Ok(serialized) = serialized else { return };
        let config: Result<ConfigDto, _> = serde_json::from_str(&serialized);
        assert!(config.is_ok(), "config should deserialize");
        let Ok(config) = config else { return };
        let recording = config.recording.as_ref();
        assert!(recording.is_some(), "explicit recording block should round-trip");
        let Some(recording) = recording else { return };
        assert!(recording.enabled);
    }

    #[test]
    fn apply_and_prepare_config_returns_the_shared_recording_validation_error() {
        let invalid_recording = RecordingConfigDto {
            default_pre_roll_secs: Some(2),
            max_pre_roll_secs: 1,
            ..RecordingConfigDto::default()
        };
        let mut config = ConfigDto::default();

        let result = apply_and_prepare_config(&mut config, vec![ConfigForm::Recording(true, invalid_recording)]);

        assert!(result.is_err(), "invalid recording config must be rejected");
        let Err(error) = result else { return };
        assert!(error.to_string().contains("default_pre_roll_secs (2) must not exceed max_pre_roll_secs (1)"));
    }
}
