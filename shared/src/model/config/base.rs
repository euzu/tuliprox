use crate::{
    error::{TuliproxError, TuliproxErrorKind},
    model::{
        ConfigApiDto, HdHomeRunConfigDto, IpCheckConfigDto, LibraryConfigDto, LogConfigDto, MessagingConfigDto,
        MetadataUpdateConfigDto, ProxyConfigDto, ReverseProxyConfigDto, ScheduleConfigDto, VideoConfigDto,
        WebUiConfigDto,
    },
    utils::{
        default_connect_timeout_secs, default_default_user_agent, default_main_backup_dir, default_main_mapping_path,
        default_main_storage_dir, default_main_template_path, default_main_user_config_dir,
        default_supported_video_extensions, is_blank_optional_string, is_blank_or_default_backup_dir,
        is_blank_or_default_mapping_path, is_blank_or_default_storage_dir, is_blank_or_default_template_path,
        is_blank_or_default_user_config_dir, is_default_connect_timeout_secs, is_false,
        is_none_or_empty_metadata_update, is_none_or_empty_video, normalize_optional_config_file_path,
        normalize_optional_dir, DEFAULT_BACKUP_DIR, DEFAULT_CUSTOM_STREAM_RESPONSE_PATH, DEFAULT_STORAGE_DIR,
        DEFAULT_USER_CONFIG_DIR, MAPPING_FILE, TEMPLATE_FILE,
    },
};

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub process_parallel: bool,
    pub api: ConfigApiDto,
    #[serde(default, alias = "working_dir", skip_serializing_if = "is_blank_or_default_storage_dir")]
    pub storage_dir: Option<String>,
    #[serde(default = "default_default_user_agent", skip_serializing_if = "is_blank_optional_string")]
    pub default_user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_or_default_backup_dir")]
    pub backup_dir: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_or_default_user_config_dir")]
    pub user_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_or_default_mapping_path")]
    pub mapping_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_or_default_template_path")]
    pub template_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub custom_stream_response_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_none_or_empty_video")]
    pub video: Option<VideoConfigDto>,
    #[serde(default, skip_serializing_if = "is_none_or_empty_metadata_update")]
    pub metadata_update: Option<MetadataUpdateConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedules: Option<Vec<ScheduleConfigDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LogConfigDto>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub user_access_control: bool,
    #[serde(default = "default_connect_timeout_secs", skip_serializing_if = "is_default_connect_timeout_secs")]
    pub connect_timeout_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_timer_mins: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub update_on_boot: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub config_hot_reload: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disk_based_processing: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub accept_insecure_ssl_certificates: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_ui: Option<WebUiConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messaging: Option<MessagingConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_proxy: Option<ReverseProxyConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdhomerun: Option<HdHomeRunConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipcheck: Option<IpCheckConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<LibraryConfigDto>,
}

impl Default for ConfigDto {
    fn default() -> Self {
        Self {
            process_parallel: false,
            api: ConfigApiDto::default(),
            storage_dir: None,
            default_user_agent: default_default_user_agent(),
            backup_dir: None,
            user_config_dir: None,
            mapping_path: None,
            template_path: None,
            custom_stream_response_path: None,
            video: None,
            metadata_update: None,
            schedules: None,
            log: None,
            user_access_control: false,
            connect_timeout_secs: default_connect_timeout_secs(),
            sleep_timer_mins: None,
            update_on_boot: false,
            config_hot_reload: false,
            disk_based_processing: false,
            accept_insecure_ssl_certificates: false,
            web_ui: None,
            messaging: None,
            reverse_proxy: None,
            hdhomerun: None,
            proxy: None,
            ipcheck: None,
            library: None,
        }
    }
}

// This MainConfigDto is a copy of ConfigDto simple fields for form editing.
// It has no other purpose than editing and saving the simple config values
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MainConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub process_parallel: bool,
    #[serde(default = "default_main_storage_dir", skip_serializing_if = "is_blank_or_default_storage_dir")]
    pub storage_dir: Option<String>,
    #[serde(default = "default_default_user_agent", skip_serializing_if = "is_blank_optional_string")]
    pub default_user_agent: Option<String>,
    #[serde(default = "default_main_backup_dir", skip_serializing_if = "is_blank_or_default_backup_dir")]
    pub backup_dir: Option<String>,
    #[serde(default = "default_main_user_config_dir", skip_serializing_if = "is_blank_or_default_user_config_dir")]
    pub user_config_dir: Option<String>,
    #[serde(default = "default_main_mapping_path", skip_serializing_if = "is_blank_or_default_mapping_path")]
    pub mapping_path: Option<String>,
    #[serde(default = "default_main_template_path", skip_serializing_if = "is_blank_or_default_template_path")]
    pub template_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub custom_stream_response_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub user_access_control: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disk_based_processing: bool,
    #[serde(default = "default_connect_timeout_secs", skip_serializing_if = "is_default_connect_timeout_secs")]
    pub connect_timeout_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_timer_mins: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub update_on_boot: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub config_hot_reload: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub accept_insecure_ssl_certificates: bool,
}

impl Default for MainConfigDto {
    fn default() -> Self {
        MainConfigDto {
            process_parallel: false,
            disk_based_processing: false,
            storage_dir: default_main_storage_dir(),
            default_user_agent: default_default_user_agent(),
            backup_dir: default_main_backup_dir(),
            user_config_dir: default_main_user_config_dir(),
            mapping_path: default_main_mapping_path(),
            template_path: default_main_template_path(),
            custom_stream_response_path: None,
            user_access_control: false,
            connect_timeout_secs: default_connect_timeout_secs(),
            sleep_timer_mins: None,
            update_on_boot: false,
            config_hot_reload: false,
            accept_insecure_ssl_certificates: false,
        }
    }
}

impl From<&ConfigDto> for MainConfigDto {
    fn from(config: &ConfigDto) -> Self {
        Self {
            process_parallel: config.process_parallel,
            disk_based_processing: config.disk_based_processing,
            storage_dir: config.storage_dir.clone(),
            default_user_agent: config.default_user_agent.clone(),
            backup_dir: config.backup_dir.clone(),
            user_config_dir: config.user_config_dir.clone(),
            mapping_path: config.mapping_path.clone(),
            template_path: config.template_path.clone(),
            custom_stream_response_path: config.custom_stream_response_path.clone(),
            user_access_control: config.user_access_control,
            connect_timeout_secs: config.connect_timeout_secs,
            sleep_timer_mins: config.sleep_timer_mins,
            update_on_boot: config.update_on_boot,
            config_hot_reload: config.config_hot_reload,
            accept_insecure_ssl_certificates: config.accept_insecure_ssl_certificates,
        }
    }
}

// This SchedulesConfigDto is a copy of ConfigDto schedules fields for form editing.
// It has no other purpose than editing and saving the schedules
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct SchedulesConfigDto {
    #[serde(default)]
    pub schedules: Option<Vec<ScheduleConfigDto>>,
}

impl SchedulesConfigDto {
    pub fn is_empty(&self) -> bool { self.schedules.as_deref().is_none_or(|s| s.is_empty()) }
}

impl From<&ConfigDto> for SchedulesConfigDto {
    fn from(config: &ConfigDto) -> Self { Self { schedules: config.schedules.clone() } }
}

pub struct HdHomeRunDeviceOverview {
    pub enabled: bool,
    pub devices: Vec<String>,
}

impl ConfigDto {
    pub fn prepare(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        self.api.prepare();

        if is_blank_optional_string(&self.default_user_agent) {
            self.default_user_agent = default_default_user_agent();
        }
        if is_blank_or_default_storage_dir(&self.storage_dir) {
            self.storage_dir = default_main_storage_dir();
        }
        if is_blank_or_default_backup_dir(&self.backup_dir) {
            self.backup_dir = default_main_backup_dir();
        }
        if is_blank_or_default_user_config_dir(&self.user_config_dir) {
            self.user_config_dir = default_main_user_config_dir();
        }
        if is_blank_or_default_mapping_path(&self.mapping_path) {
            self.mapping_path = default_main_mapping_path();
        }
        if is_blank_or_default_template_path(&self.template_path) {
            self.template_path = default_main_template_path();
        }

        if let Some(mins) = self.sleep_timer_mins {
            if mins == 0 {
                return Err(TuliproxError::new(
                    TuliproxErrorKind::Info,
                    "`sleep_timer_mins` must be > 0 when specified".to_string(),
                ));
            }
        }

        self.prepare_web()?;
        self.prepare_hdhomerun(include_computed)?;
        self.prepare_video_config()?;
        self.prepare_metadata_update_config()?;

        if let Some(reverse_proxy) = self.reverse_proxy.as_mut() {
            reverse_proxy.prepare(self.storage_dir.as_deref().unwrap_or_default())?;
        }
        if let Some(proxy) = &mut self.proxy {
            proxy.prepare()?;
        }
        if let Some(ipcheck) = self.ipcheck.as_mut() {
            ipcheck.prepare()?;
        }

        if let Some(messaging) = &mut self.messaging {
            messaging.prepare(include_computed)?;
        }
        if let Some(library) = &mut self.library {
            library.playlist.prepare();
        }

        Ok(())
    }

    fn prepare_web(&mut self) -> Result<(), TuliproxError> {
        if let Some(web_ui_config) = self.web_ui.as_mut() {
            web_ui_config.prepare()?;
        }
        Ok(())
    }

    fn prepare_hdhomerun(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        if let Some(hdhomerun) = &mut self.hdhomerun {
            if hdhomerun.enabled {
                hdhomerun.prepare(self.api.port, include_computed)?;
            }
        }
        Ok(())
    }

    fn prepare_video_config(&mut self) -> Result<(), TuliproxError> {
        match &mut self.video {
            None => {
                self.video = Some(VideoConfigDto {
                    extensions: default_supported_video_extensions(),
                    download: None,
                    web_search: None,
                });
            }
            Some(video) => match video.prepare() {
                Ok(()) => {}
                Err(err) => return Err(err),
            },
        }
        Ok(())
    }

    fn prepare_metadata_update_config(&mut self) -> Result<(), TuliproxError> {
        let mut metadata_update = self.metadata_update.clone().unwrap_or_default();

        metadata_update.prepare()?;

        if metadata_update.is_empty() {
            self.metadata_update = None;
        } else {
            self.metadata_update = Some(metadata_update);
        }

        Ok(())
    }

    pub fn is_valid(&self) -> bool {
        if self.api.host.is_empty() {
            return false;
        }

        if let Some(video) = &self.video {
            if let Some(download) = &video.download {
                if let Some(episode_pattern) = &download.episode_pattern {
                    if !episode_pattern.is_empty() {
                        let re = crate::model::REGEX_CACHE.get_or_compile(episode_pattern);
                        if re.is_err() {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    pub fn get_hdhr_device_overview(&self) -> Option<HdHomeRunDeviceOverview> {
        self.hdhomerun.as_ref().map(|hdhr| HdHomeRunDeviceOverview {
            enabled: hdhr.enabled,
            devices: hdhr.devices.iter().map(|d| d.name.to_string()).collect::<Vec<String>>(),
        })
    }

    pub fn update_from_main_config(&mut self, main_config: &MainConfigDto) {
        self.process_parallel = main_config.process_parallel;
        self.disk_based_processing = main_config.disk_based_processing;
        self.storage_dir = normalize_optional_dir(&main_config.storage_dir, DEFAULT_STORAGE_DIR);
        self.default_user_agent = main_config.default_user_agent.clone();
        self.backup_dir = normalize_optional_dir(&main_config.backup_dir, DEFAULT_BACKUP_DIR);
        self.user_config_dir = normalize_optional_dir(&main_config.user_config_dir, DEFAULT_USER_CONFIG_DIR);
        self.mapping_path = normalize_optional_config_file_path(&main_config.mapping_path, MAPPING_FILE);
        self.template_path = normalize_optional_config_file_path(&main_config.template_path, TEMPLATE_FILE);
        self.custom_stream_response_path =
            normalize_optional_dir(&main_config.custom_stream_response_path, DEFAULT_CUSTOM_STREAM_RESPONSE_PATH);
        self.user_access_control = main_config.user_access_control;
        self.connect_timeout_secs = main_config.connect_timeout_secs;
        self.sleep_timer_mins = main_config.sleep_timer_mins;
        self.update_on_boot = main_config.update_on_boot;
        self.config_hot_reload = main_config.config_hot_reload;
        self.accept_insecure_ssl_certificates = main_config.accept_insecure_ssl_certificates;
    }

    pub fn is_geoip_enabled(&self) -> bool {
        self.reverse_proxy.as_ref().is_some_and(|r| r.geoip.as_ref().is_some_and(|g| g.enabled))
    }

    pub fn is_library_enabled(&self) -> bool { self.library.as_ref().is_some_and(|l| l.enabled) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::{default_supported_video_extensions, CONFIG_PATH};
    use serde_json::json;

    #[test]
    fn default_uses_connect_timeout_default_value() {
        let cfg = ConfigDto::default();
        assert_eq!(cfg.connect_timeout_secs, default_connect_timeout_secs());
    }

    #[test]
    fn serializing_skips_video_for_default_values() {
        let cfg = ConfigDto {
            video: Some(VideoConfigDto {
                extensions: default_supported_video_extensions(),
                download: None,
                web_search: None,
            }),
            ..ConfigDto::default()
        };

        let serialized = serde_json::to_string(&cfg).expect("config serialization should succeed");
        assert!(!serialized.contains("\"video\""), "expected no video field, got: {serialized}");
    }

    #[test]
    fn serializing_keeps_video_for_non_default_values() {
        let cfg = ConfigDto {
            video: Some(VideoConfigDto {
                extensions: default_supported_video_extensions(),
                download: None,
                web_search: Some("https://example.org?q={}".to_string()),
            }),
            ..ConfigDto::default()
        };

        let serialized = serde_json::to_string(&cfg).expect("config serialization should succeed");
        assert!(serialized.contains("\"video\""), "expected video field, got: {serialized}");
    }

    #[test]
    fn serializing_skips_default_storage_backup_and_user_config_dirs() {
        let cfg = ConfigDto {
            storage_dir: Some(DEFAULT_STORAGE_DIR.to_string()),
            backup_dir: Some(DEFAULT_BACKUP_DIR.to_string()),
            user_config_dir: Some(DEFAULT_USER_CONFIG_DIR.to_string()),
            ..ConfigDto::default()
        };

        let serialized = serde_json::to_string(&cfg).expect("config serialization should succeed");
        assert!(
            !serialized.contains("\"storage_dir\""),
            "expected no storage_dir field for default value, got: {serialized}"
        );
        assert!(
            !serialized.contains("\"backup_dir\""),
            "expected no backup_dir field for default value, got: {serialized}"
        );
        assert!(
            !serialized.contains("\"user_config_dir\""),
            "expected no user_config_dir field for default value, got: {serialized}"
        );
    }

    #[test]
    fn serializing_keeps_non_default_storage_and_backup_dirs() {
        let cfg = ConfigDto {
            storage_dir: Some("custom-storage".to_string()),
            backup_dir: Some("custom-backup".to_string()),
            user_config_dir: Some("custom-user-config".to_string()),
            ..ConfigDto::default()
        };

        let serialized = serde_json::to_string(&cfg).expect("config serialization should succeed");
        assert!(
            serialized.contains("\"storage_dir\""),
            "expected storage_dir field for non-default value, got: {serialized}"
        );
        assert!(
            serialized.contains("\"backup_dir\""),
            "expected backup_dir field for non-default value, got: {serialized}"
        );
        assert!(
            serialized.contains("\"user_config_dir\""),
            "expected user_config_dir field for non-default value, got: {serialized}"
        );
    }

    #[test]
    fn main_config_from_applies_default_storage_backup_and_user_config_dirs() {
        let mut cfg = ConfigDto::default();
        cfg.prepare(false).expect("prepare should succeed");
        let main = MainConfigDto::from(&cfg);
        assert_eq!(main.storage_dir.as_deref(), Some(DEFAULT_STORAGE_DIR));
        assert_eq!(main.backup_dir.as_deref(), Some(DEFAULT_BACKUP_DIR));
        assert_eq!(main.user_config_dir.as_deref(), Some(DEFAULT_USER_CONFIG_DIR));
        assert_eq!(main.mapping_path.as_deref(), Some(format!("./{CONFIG_PATH}/{MAPPING_FILE}").as_str()));
        assert_eq!(main.template_path.as_deref(), Some(format!("./{CONFIG_PATH}/{TEMPLATE_FILE}").as_str()));
    }

    #[test]
    fn update_from_main_config_omits_default_optional_paths() {
        let mut cfg = ConfigDto::default();
        let main = MainConfigDto {
            storage_dir: Some(DEFAULT_STORAGE_DIR.to_string()),
            backup_dir: Some(DEFAULT_BACKUP_DIR.to_string()),
            user_config_dir: Some(DEFAULT_USER_CONFIG_DIR.to_string()),
            mapping_path: Some(format!("./{CONFIG_PATH}/{MAPPING_FILE}")),
            template_path: Some(format!("./{CONFIG_PATH}/{TEMPLATE_FILE}")),
            ..MainConfigDto::default()
        };

        cfg.update_from_main_config(&main);
        assert!(cfg.storage_dir.is_none());
        assert!(cfg.backup_dir.is_none());
        assert!(cfg.user_config_dir.is_none());
        assert!(cfg.mapping_path.is_none());
        assert!(cfg.template_path.is_none());
    }

    #[test]
    fn prepare_sets_default_optional_paths() {
        let mut cfg = ConfigDto {
            storage_dir: None,
            backup_dir: None,
            user_config_dir: None,
            mapping_path: None,
            template_path: None,
            ..ConfigDto::default()
        };
        cfg.prepare(false).expect("prepare should succeed");
        assert_eq!(cfg.storage_dir.as_deref(), Some(DEFAULT_STORAGE_DIR));
        assert_eq!(cfg.backup_dir.as_deref(), Some(DEFAULT_BACKUP_DIR));
        assert_eq!(cfg.user_config_dir.as_deref(), Some(DEFAULT_USER_CONFIG_DIR));
        assert_eq!(cfg.mapping_path.as_deref(), Some(format!("./{CONFIG_PATH}/{MAPPING_FILE}").as_str()));
        assert_eq!(cfg.template_path.as_deref(), Some(format!("./{CONFIG_PATH}/{TEMPLATE_FILE}").as_str()));
    }

    #[test]
    fn deserializing_rejects_legacy_video_ffprobe_fields() {
        let raw = json!({
            "api": {
                "host": "127.0.0.1",
                "port": 8901,
                "web_root": "./web"
            },
            "storage_dir": ".",
            "video": {
                "extensions": ["mp4"],
                "ffprobe_enabled": true
            }
        });

        let result: Result<ConfigDto, _> = serde_json::from_value(raw);
        assert!(result.is_err(), "legacy ffprobe field under video must fail");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ffprobe_enabled"), "unexpected error text: {err}");
    }

    #[test]
    fn deserializing_rejects_legacy_data_dir_alias() {
        let raw = json!({
            "api": {
                "host": "127.0.0.1",
                "port": 8901,
                "web_root": "./web"
            },
            "data_dir": "."
        });

        let result: Result<ConfigDto, _> = serde_json::from_value(raw);
        assert!(result.is_err(), "data_dir should not deserialize");
    }

    #[test]
    fn deserializing_accepts_legacy_working_dir_alias() {
        let raw = json!({
            "api": {
                "host": "127.0.0.1",
                "port": 8901,
                "web_root": "./web"
            },
            "working_dir": "."
        });

        let cfg: ConfigDto = serde_json::from_value(raw).expect("working_dir should deserialize as legacy alias");
        assert_eq!(cfg.storage_dir.as_deref(), Some("."));
    }

    #[test]
    fn deserializing_accepts_missing_storage_dir() {
        let raw = json!({
            "api": {
                "host": "127.0.0.1",
                "port": 8901,
                "web_root": "./web"
            }
        });

        let cfg: ConfigDto = serde_json::from_value(raw).expect("missing storage_dir should deserialize");
        assert!(cfg.storage_dir.is_none());
    }
}
