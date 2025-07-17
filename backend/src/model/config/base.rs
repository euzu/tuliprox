use std::path::{Path, PathBuf};
use log::{error, info};
use path_clean::PathClean;
use shared::error::{TuliproxError};
use shared::model::{ConfigDto, HdHomeRunDeviceOverview};
use shared::utils::set_sanitize_sensitive_info;
use crate::model::{macros, ConfigApi, ReverseProxyConfig, ScheduleConfig};
use crate::model::{HdHomeRunConfig, IpCheckConfig, LogConfig, MessagingConfig, ProxyConfig, VideoConfig, WebUiConfig};
use crate::{utils};


fn create_directories(cfg: &Config, temp_path: &Path) {
    // Collect the paths into a vector.
    let paths_strings = [
        Some(cfg.working_dir.clone()),
        cfg.backup_dir.clone(),
        cfg.user_config_dir.clone(),
        cfg.video.as_ref().and_then(|v| v.download.as_ref()).and_then(|d| d.directory.clone()),
        cfg.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref().and_then(|c| if c.enabled { Some(c.dir.to_string()) } else { None }))
    ];

    let mut paths: Vec<PathBuf> = paths_strings.iter()
        .filter_map(|opt| opt.as_ref()) // Get rid of the `Option`
        .map(PathBuf::from).collect();
    paths.push(temp_path.to_path_buf());

    // Iterate over the paths, filter out `None` values, and process the `Some(path)` values.
    for path in &paths {
        if !path.exists() {
            // Create the directory tree if it doesn't exist
            let path_value = path.to_str().unwrap_or("?");
            if let Err(e) = std::fs::create_dir_all(path) {
                error!("Failed to create directory {path_value}: {e}");
            } else {
                info!("Created directory: {path_value}");
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub threads: u8,
    pub api: ConfigApi,
    pub working_dir: String,
    pub backup_dir: Option<String>,
    pub user_config_dir: Option<String>,
    pub mapping_path: Option<String>,
    pub custom_stream_response_path: Option<String>,
    pub video: Option<VideoConfig>,
    pub schedules: Option<Vec<ScheduleConfig>>,
    pub log: Option<LogConfig>,
    pub user_access_control: bool,
    pub connect_timeout_secs: u32,
    pub sleep_timer_mins: Option<u32>,
    pub update_on_boot: bool,
    pub config_hot_reload: bool,
    pub web_ui: Option<WebUiConfig>,
    pub messaging: Option<MessagingConfig>,
    pub reverse_proxy: Option<ReverseProxyConfig>,
    pub hdhomerun: Option<HdHomeRunConfig>,
    pub proxy: Option<ProxyConfig>,
    pub ipcheck: Option<IpCheckConfig>,
}

impl Config {
    pub fn prepare(&mut self, config_path: &str) -> Result<(), TuliproxError> {
        let work_dir = &self.working_dir;
        self.working_dir = utils::resolve_directory_path(work_dir);

        self.prepare_directories();
        self.prepare_api_web_root();
        if let Some(ref mut webui) = &mut self.web_ui {
            webui.prepare(config_path)?;
        }

        Ok(())
    }

    fn prepare_directories(&mut self) {
        fn set_directory(path: &mut Option<String>, default_subdir: &str, working_dir: &str) {
            *path = Some(match path.as_ref() {
                Some(existing) => existing.to_owned(),
                None => PathBuf::from(working_dir).join(default_subdir).clean().to_string_lossy().to_string(),
            });
        }

        set_directory(&mut self.backup_dir, "backup", &self.working_dir);
        set_directory(&mut self.user_config_dir, "user_config", &self.working_dir);
    }

    fn prepare_api_web_root(&mut self) {
        if !self.api.web_root.is_empty() {
            self.api.web_root = utils::make_absolute_path(&self.api.web_root, &self.working_dir);
        }
    }

    pub fn update_runtime(&self) {
        set_sanitize_sensitive_info(self.log.as_ref().is_none_or(|l| l.sanitize_sensitive_info));
        let temp_path = PathBuf::from(&self.working_dir).join("tmp");
        create_directories(self, &temp_path);
        let _ = tempfile::env::override_temp_dir(&temp_path);
    }

    pub fn get_hdhr_device_overview(&self) -> Option<HdHomeRunDeviceOverview> {
        self.hdhomerun.as_ref().map(|hdhr|
            HdHomeRunDeviceOverview {
                enabled: hdhr.enabled,
                devices: hdhr.devices.iter().map(|d| d.name.to_string()).collect::<Vec<String>>(),
            })
    }
}

macros::from_impl!(Config);

impl From<&ConfigDto> for Config {
    fn from(dto: &ConfigDto) -> Self {
        Config {
            threads: 0,
            api: ConfigApi::from(&dto.api),
            working_dir: dto.working_dir.to_string(),
            backup_dir: dto.backup_dir.clone(),
            user_config_dir: dto.user_config_dir.clone(),
            mapping_path: dto.mapping_path.clone(),
            custom_stream_response_path: dto.custom_stream_response_path.clone(),
            video: dto.video.as_ref().map(Into::into),
            schedules: dto.schedules.as_ref().map(|s| s.iter().map(Into::into).collect()),
            log: dto.log.as_ref().map(Into::into),
            user_access_control: dto.user_access_control,
            connect_timeout_secs: dto.connect_timeout_secs,
            sleep_timer_mins: dto.sleep_timer_mins,
            update_on_boot: dto.update_on_boot,
            config_hot_reload: dto.config_hot_reload,
            web_ui: dto.web_ui.as_ref().map(Into::into),
            messaging: dto.messaging.as_ref().map(Into::into),
            reverse_proxy: dto.reverse_proxy.as_ref().map(Into::into),
            hdhomerun: dto.hdhomerun.as_ref().map(Into::into),
            proxy: dto.proxy.as_ref().map(Into::into),
            ipcheck: dto.ipcheck.as_ref().map(Into::into),
        }
    }
}