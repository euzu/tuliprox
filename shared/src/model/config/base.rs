use crate::{
    error::{TuliproxError, TuliproxErrorKind},
    model::{
        ConfigApiDto, HdHomeRunConfigDto, IpCheckConfigDto, LibraryConfigDto, LogConfigDto, MessagingConfigDto,
        ProxyConfigDto, ReverseProxyConfigDto, ScheduleConfigDto, VideoConfigDto, WebUiConfigDto,
    },
    utils::{
        default_connect_timeout_secs, default_supported_video_extensions, is_blank_optional_string,
        is_default_connect_timeout_secs, is_false,
    },
};

pub const DEFAULT_USER_AGENT: &str = "VLC/3.0.16 LibVLC/3.0.16";

fn default_default_user_agent() -> Option<String> { Some(DEFAULT_USER_AGENT.to_string()) }
fn is_none_or_empty_video(video: &Option<VideoConfigDto>) -> bool {
    video.as_ref().is_none_or(VideoConfigDto::is_empty)
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub process_parallel: bool,
    pub api: ConfigApiDto,
    pub working_dir: String,
    #[serde(default = "default_default_user_agent", skip_serializing_if = "is_blank_optional_string")]
    pub default_user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub backup_dir: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub user_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub mapping_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub template_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub custom_stream_response_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_none_or_empty_video")]
    pub video: Option<VideoConfigDto>,
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
            working_dir: String::new(),
            default_user_agent: default_default_user_agent(),
            backup_dir: None,
            user_config_dir: None,
            mapping_path: None,
            template_path: None,
            custom_stream_response_path: None,
            video: None,
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
    pub working_dir: String,
    #[serde(default = "default_default_user_agent", skip_serializing_if = "is_blank_optional_string")]
    pub default_user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub backup_dir: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub user_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub mapping_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
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
            working_dir: String::new(),
            default_user_agent: default_default_user_agent(),
            backup_dir: None,
            user_config_dir: None,
            mapping_path: None,
            template_path: None,
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
            working_dir: config.working_dir.clone(),
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
        if is_blank_optional_string(&self.default_user_agent) {
            self.default_user_agent = default_default_user_agent();
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

        if let Some(reverse_proxy) = self.reverse_proxy.as_mut() {
            reverse_proxy.prepare(&self.working_dir)?;
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
                    ffprobe_enabled: false,
                    ffprobe_timeout: None,
                });
            }
            Some(video) => match video.prepare() {
                Ok(()) => {}
                Err(err) => return Err(err),
            },
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
        self.working_dir = main_config.working_dir.clone();
        self.default_user_agent = main_config.default_user_agent.clone();
        self.backup_dir = main_config.backup_dir.clone();
        self.user_config_dir = main_config.user_config_dir.clone();
        self.mapping_path = main_config.mapping_path.clone();
        self.template_path = main_config.template_path.clone();
        self.custom_stream_response_path = main_config.custom_stream_response_path.clone();
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
    use crate::utils::default_supported_video_extensions;

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
                ffprobe_enabled: false,
                ffprobe_timeout: None,
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
                web_search: None,
                ffprobe_enabled: true,
                ffprobe_timeout: None,
            }),
            ..ConfigDto::default()
        };

        let serialized = serde_json::to_string(&cfg).expect("config serialization should succeed");
        assert!(serialized.contains("\"video\""), "expected video field, got: {serialized}");
    }
}
