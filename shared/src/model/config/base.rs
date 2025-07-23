use crate::error::{TuliproxError, TuliproxErrorKind};
use crate::model::{ConfigApiDto, HdHomeRunConfigDto, IpCheckConfigDto, LogConfigDto, MessagingConfigDto, ProxyConfigDto, ReverseProxyConfigDto, ScheduleConfigDto, VideoConfigDto, WebUiConfigDto, DEFAULT_VIDEO_EXTENSIONS};
use crate::utils::default_connect_timeout_secs;

pub const DEFAULT_USER_AGENT: &str = "VLC/3.0.16 LibVLC/3.0.16";

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigDto {
    #[serde(default)]
    pub threads: u8,
    pub api: ConfigApiDto,
    pub working_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_stream_response_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedules: Option<Vec<ScheduleConfigDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LogConfigDto>,
    #[serde(default)]
    pub user_access_control: bool,
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_timer_mins: Option<u32>,
    #[serde(default)]
    pub update_on_boot: bool,
    #[serde(default)]
    pub config_hot_reload: bool,
    #[serde(default)]
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
}

pub struct HdHomeRunDeviceOverview {
    pub enabled: bool,
    pub devices: Vec<String>,
}

impl ConfigDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if let Some(mins) = self.sleep_timer_mins {
            if mins == 0 {
                return Err(TuliproxError::new(TuliproxErrorKind::Info, "`sleep_timer_mins` must be > 0 when specified".to_string()));
            }
        }

        self.api.prepare();
        self.prepare_web()?;
        self.prepare_hdhomerun()?;
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

        Ok(())
    }

    fn prepare_web(&mut self) -> Result<(), TuliproxError> {
        if let Some(web_ui_config) = self.web_ui.as_mut() {
            web_ui_config.prepare()?;
        }
        Ok(())
    }

    fn prepare_hdhomerun(&mut self) -> Result<(), TuliproxError> {
        if let Some(hdhomerun) = &mut self.hdhomerun {
            if hdhomerun.enabled {
                hdhomerun.prepare(self.api.port)?;
            }
        }
        Ok(())
    }

    fn prepare_video_config(&mut self) -> Result<(), TuliproxError> {
        match &mut self.video {
            None => {
                self.video = Some(VideoConfigDto {
                    extensions: DEFAULT_VIDEO_EXTENSIONS.iter().map(ToString::to_string).collect(),
                    download: None,
                    web_search: None,
                });
            }
            Some(video) => {
                match video.prepare() {
                    Ok(()) => {}
                    Err(err) => return Err(err)
                }
            }
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
                        let re = regex::Regex::new(episode_pattern);
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
        self.hdhomerun.as_ref().map(|hdhr|
            HdHomeRunDeviceOverview {
                enabled: hdhr.enabled,
                devices: hdhr.devices.iter().map(|d| d.name.to_string()).collect::<Vec<String>>(),
            })
    }
}