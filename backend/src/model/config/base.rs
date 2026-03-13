use crate::model::{
    macros, ConfigApi, HdHomeRunConfig, HdHomeRunFlags, IpCheckConfig, LibraryConfig, LogConfig, MetadataUpdateConfig,
    MessagingConfig, ProxyConfig, ReverseProxyConfig, ReverseProxyDisabledHeaderConfig, ScheduleConfig, VideoConfig,
    WebUiConfig,
};
use crate::utils;
use log::{error, info};
use path_clean::PathClean;
use shared::error::TuliproxError;
use shared::model::{ConfigDto, HdHomeRunDeviceOverview};
use shared::utils::{default_grace_period_millis, default_grace_period_timeout_secs, set_sanitize_sensitive_info, DEFAULT_BACKUP_DIR, DEFAULT_CACHE_DIR, DEFAULT_DOWNLOAD_DIR, DEFAULT_STORAGE_DIR, DEFAULT_STORAGE_TEMP_DIR, DEFAULT_USER_CONFIG_DIR};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use crate::utils::get_default_path_for_home;

fn create_directories(cfg: &Config, temp_path: &Path) {
    // Collect the paths into a vector.
    let paths_strings = [
        Some(cfg.storage_dir.clone()),
        cfg.backup_dir.clone(),
        cfg.user_config_dir.clone(),
        cfg.video.as_ref().and_then(|v| v.download.as_ref()).map(|d| d.directory.clone()),
        cfg.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref().and_then(|c| if c.enabled { Some(c.directory.clone()) } else { None })),
        cfg.metadata_update.as_ref().map(|m| m.cache_path.clone()),
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

#[derive(Clone, Copy)]
pub struct GracePeriodOptions {
    pub period_millis: u64,
    pub timeout_secs: u64,
    pub hold_stream: bool,
}

impl Default for GracePeriodOptions {
    fn default() -> Self {
        Self {
            period_millis: default_grace_period_millis(),
            timeout_secs: default_grace_period_timeout_secs(),
            hold_stream: true,
        }
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub process_parallel: bool,
    pub api: ConfigApi,
    pub storage_dir: String,
    pub default_user_agent: Option<String>,
    pub backup_dir: Option<String>,
    pub user_config_dir: Option<String>,
    pub mapping_path: Option<String>,
    pub template_path: Option<String>,
    pub custom_stream_response_path: Option<String>,
    pub custom_stream_response_timeout_secs: u32,
    pub video: Option<VideoConfig>,
    pub metadata_update: Option<MetadataUpdateConfig>,
    pub schedules: Option<Vec<ScheduleConfig>>,
    pub log: Option<LogConfig>,
    pub user_access_control: bool,
    pub connect_timeout_secs: u32,
    pub sleep_timer_mins: Option<u32>,
    pub update_on_boot: bool,
    pub config_hot_reload: bool,
    pub disk_based_processing: bool,
    pub accept_insecure_ssl_certificates: bool,
    pub web_ui: Option<WebUiConfig>,
    pub messaging: Option<MessagingConfig>,
    pub reverse_proxy: Option<ReverseProxyConfig>,
    pub hdhomerun: Option<HdHomeRunConfig>,
    pub proxy: Option<ProxyConfig>,
    pub ipcheck: Option<IpCheckConfig>,
    pub library: Option<LibraryConfig>,
}

impl Config {
    fn lexical_normalize(path: &Path) -> PathBuf {
        path.components().collect::<PathBuf>()
    }

    fn normalize_storage_path(raw_storage_dir: &str, storage_dir: &str, input: &str, default_dir: &str) -> Option<PathBuf> {
        let trimmed_input = input.trim();
        if trimmed_input.is_empty() {
            return Some(PathBuf::from(storage_dir).join(default_dir));
        }

        let input_path = PathBuf::from(trimmed_input);
        if input_path.is_absolute() {
            return None;
        }

        let mut normalized_relative = Self::lexical_normalize(&input_path);
        let trimmed_raw_storage = raw_storage_dir.trim();
        if !trimmed_raw_storage.is_empty() {
            let raw_path = PathBuf::from(trimmed_raw_storage);
            if raw_path.is_relative() {
                let normalized_raw = Self::lexical_normalize(&raw_path);
                if normalized_raw.components().next().is_some() {
                    while normalized_relative.starts_with(&normalized_raw) {
                        match normalized_relative.strip_prefix(&normalized_raw) {
                            Ok(stripped) => {
                                normalized_relative = Self::lexical_normalize(stripped);
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        }

        Some(PathBuf::from(storage_dir).join(normalized_relative))
    }

    pub fn prepare(&mut self, config_path: &str, home_path: &str) -> Result<(), TuliproxError> {

        self.prepare_directories(home_path);
        if let Some(ref mut webui) = &mut self.web_ui {
            webui.prepare(config_path)?;
        }

        if let Some(library) = self.library.as_mut() {
            library.prepare(&self.storage_dir)?;
        }

        if let Some(metadata_update) = self.metadata_update.as_mut() {
            let meta_path = PathBuf::from(&metadata_update.cache_path);
            let meta_path = if meta_path.is_relative() {
                PathBuf::from(&self.storage_dir).join(meta_path)
            } else {
                meta_path
            };
            metadata_update.cache_path = meta_path.to_string_lossy().to_string();
        }

        if let Some(messaging) = self.messaging.as_mut() {
            messaging.prepare(config_path);
        }

        if let Some(video) = self.video.as_mut() {
            video.prepare();
            if let Some(download) = video.download.as_mut() {
                let download_path = PathBuf::from(&download.directory);
                if download.directory.trim().is_empty() {
                    download.directory = get_default_path_for_home(Path::new(home_path), DEFAULT_DOWNLOAD_DIR)
                        .clean()
                        .to_string_lossy()
                        .to_string();
                } else if download_path.is_relative() {
                    download.directory = PathBuf::from(home_path)
                        .join(download_path)
                        .clean()
                        .to_string_lossy()
                        .to_string();
                }
            }
        }

        Ok(())
    }

    fn prepare_reverse_proxy_cache_dir(&mut self, raw_storage_dir: &str) {
        let Some(reverse_proxy) = self.reverse_proxy.as_mut() else {
            return;
        };
        let Some(cache) = reverse_proxy.cache.as_mut() else {
            return;
        };
        if !cache.enabled {
            return;
        }

        let normalized = Self::normalize_storage_path(raw_storage_dir, &self.storage_dir, &cache.directory, DEFAULT_CACHE_DIR)
            .unwrap_or_else(|| PathBuf::from(cache.directory.trim()));
        cache.directory = normalized.clean().to_string_lossy().to_string();
    }

    fn prepare_directories(&mut self, home_path: &str) {
        let raw_storage_dir = self.storage_dir.trim().to_string();
        let storage_dir_path = if raw_storage_dir.is_empty() {
            get_default_path_for_home(Path::new(home_path), DEFAULT_STORAGE_DIR)
        } else {
            let configured_storage_path = PathBuf::from(&raw_storage_dir);
            if configured_storage_path.is_relative() {
                PathBuf::from(home_path).join(configured_storage_path)
            } else {
                configured_storage_path
            }
        };
        self.storage_dir = utils::resolve_directory_path(storage_dir_path.to_string_lossy().as_ref());
        self.prepare_reverse_proxy_cache_dir(&raw_storage_dir);

        let storage_dir = self.storage_dir.clone();
        let normalize_optional_path = |value: Option<&str>, default_dir: &str| -> String {
            let configured = value.unwrap_or_default();
            let normalized = Self::normalize_storage_path(&raw_storage_dir, &storage_dir, configured, default_dir)
                .unwrap_or_else(|| PathBuf::from(configured.trim()));
            normalized.clean().to_string_lossy().to_string()
        };

        self.backup_dir = Some(normalize_optional_path(self.backup_dir.as_deref(), DEFAULT_BACKUP_DIR));
        self.user_config_dir = Some(normalize_optional_path(self.user_config_dir.as_deref(), DEFAULT_USER_CONFIG_DIR));
        self.prepare_api_web_root(home_path);
    }

    pub fn get_backup_dir(&self) -> Cow<'_, str> {
        self.backup_dir.as_ref().map_or_else(|| Cow::Borrowed(DEFAULT_BACKUP_DIR), |v| Cow::Borrowed(v))
    }

    fn prepare_api_web_root(&mut self, home_path: &str) {
        let web_root = self.api.web_root.trim();
        if web_root.is_empty() {
            self.api.web_root = utils::get_default_web_root_path_for_home(Path::new(home_path))
                .display()
                .to_string();
        } else {
            let web_root_path = PathBuf::from(web_root);
            let resolved = if web_root_path.is_absolute() {
                web_root_path
            } else {
                PathBuf::from(home_path).join(web_root_path).clean()
            };
            self.api.web_root = resolved.to_string_lossy().to_string();
        }
    }

    pub fn update_runtime(&self) {
        set_sanitize_sensitive_info(self.log.as_ref().is_none_or(|l| l.sanitize_sensitive_info));
        let temp_path = PathBuf::from(&self.storage_dir).join(DEFAULT_STORAGE_TEMP_DIR);
        create_directories(self, &temp_path);
        let _ = tempfile::env::override_temp_dir(&temp_path);
    }

    pub fn get_hdhr_device_overview(&self) -> Option<HdHomeRunDeviceOverview> {
        self.hdhomerun.as_ref().map(|hdhr|
            HdHomeRunDeviceOverview {
                enabled: hdhr.flags.contains(HdHomeRunFlags::Enabled),
                devices: hdhr.devices.iter().map(|d| d.name.clone()).collect::<Vec<String>>(),
            })
    }

    pub fn is_geoip_enabled(&self) -> bool {
        self.reverse_proxy.as_ref().is_some_and(|r| r.geoip.as_ref().is_some_and(|g| g.enabled))
    }

    pub fn get_disabled_headers(&self) -> Option<ReverseProxyDisabledHeaderConfig> {
        self.reverse_proxy
            .as_ref()
            .and_then(|r| r.disabled_header.clone())
    }

    pub fn get_grace_options(&self) -> GracePeriodOptions {
        self.reverse_proxy
            .as_ref()
            .and_then(|r| r.stream.as_ref())
            .map_or_else(GracePeriodOptions::default,
                         |s| GracePeriodOptions {
                             period_millis: s.grace_period_millis,
                             timeout_secs: s.grace_period_timeout_secs,
                             hold_stream: s.grace_period_hold_stream,
                         })
    }
}

macros::from_impl!(Config);

impl From<&ConfigDto> for Config {
    fn from(dto: &ConfigDto) -> Self {
        Config {
            process_parallel: dto.process_parallel,
            disk_based_processing: dto.disk_based_processing,
            api: ConfigApi::from(&dto.api),
            storage_dir: dto.storage_dir.clone().unwrap_or_default(),
            default_user_agent: dto.default_user_agent.clone(),
            backup_dir: dto.backup_dir.clone(),
            user_config_dir: dto.user_config_dir.clone(),
            mapping_path: dto.mapping_path.clone(),
            template_path: dto.template_path.clone(),
            custom_stream_response_path: dto.custom_stream_response_path.clone(),
            custom_stream_response_timeout_secs: dto.custom_stream_response_timeout_secs,
            video: dto.video.as_ref().map(Into::into),
            metadata_update: dto.metadata_update.as_ref().map(Into::into),
            schedules: dto.schedules.as_ref().map(|s| s.iter().map(Into::into).collect()),
            log: dto.log.as_ref().map(Into::into),
            user_access_control: dto.user_access_control,
            connect_timeout_secs: dto.connect_timeout_secs,
            sleep_timer_mins: dto.sleep_timer_mins,
            update_on_boot: dto.update_on_boot,
            config_hot_reload: dto.config_hot_reload,
            accept_insecure_ssl_certificates: dto.accept_insecure_ssl_certificates,
            web_ui: dto.web_ui.as_ref().map(Into::into),
            messaging: dto.messaging.as_ref().map(Into::into),
            reverse_proxy: dto.reverse_proxy.as_ref().map(Into::into),
            hdhomerun: dto.hdhomerun.as_ref().map(Into::into),
            proxy: dto.proxy.as_ref().map(Into::into),
            ipcheck: dto.ipcheck.as_ref().map(Into::into),
            library: dto.library.as_ref().map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use tempfile::tempdir;

    #[test]
    fn prepare_resolves_relative_web_root_from_home_path() {
        let temp_dir = tempdir().expect("failed to create temp directory");
        let home_path = temp_dir.path().join("home");
        let config_path = temp_dir.path().join("config");

        let mut config = Config {
            storage_dir: "data".to_string(),
            ..Default::default()
        };
        config.api.web_root = "./web".to_string();

        config
            .prepare(
                config_path.to_string_lossy().as_ref(),
                home_path.to_string_lossy().as_ref(),
            )
            .expect("prepare should succeed");

        assert_eq!(
            config.api.web_root,
            home_path.join("web").to_string_lossy()
        );
    }

    #[test]
    fn prepare_keeps_absolute_web_root_unchanged() {
        let temp_dir = tempdir().expect("failed to create temp directory");
        let home_path = temp_dir.path().join("home");
        let config_path = temp_dir.path().join("config");
        let absolute_web_root = temp_dir.path().join("custom_web_root");

        let mut config = Config {
            storage_dir: "data".to_string(),
            ..Default::default()
        };
        config.api.web_root = absolute_web_root.to_string_lossy().to_string();

        config
            .prepare(
                config_path.to_string_lossy().as_ref(),
                home_path.to_string_lossy().as_ref(),
            )
            .expect("prepare should succeed");

        assert_eq!(
            config.api.web_root,
            absolute_web_root.to_string_lossy()
        );
    }
}
