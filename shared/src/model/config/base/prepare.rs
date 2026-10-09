use super::{ConfigDto, HdHomeRunDeviceOverview, MainConfigDto};
use crate::{
    defaults::{
        default_custom_stream_response_error_status, default_default_user_agent, default_main_backup_dir,
        default_main_mapping_path, default_main_storage_dir, default_main_template_path, default_main_user_config_dir,
        default_supported_video_extensions, is_blank_or_default_backup_dir, is_blank_or_default_mapping_path,
        is_blank_or_default_storage_dir, is_blank_or_default_template_path, is_blank_or_default_user_config_dir,
        normalize_optional_config_file_path, normalize_optional_dir, DEFAULT_BACKUP_DIR,
        DEFAULT_CUSTOM_STREAM_RESPONSE_PATH, DEFAULT_STORAGE_DIR, DEFAULT_USER_CONFIG_DIR, MAPPING_FILE, TEMPLATE_FILE,
    },
    error::TuliproxError,
    model::VideoConfigDto,
    utils::is_blank_optional_string,
};

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
                return Err(TuliproxError::ConfigBase("`sleep_timer_mins` must be > 0 when specified".to_string()));
            }
        }
        if self.interner_gc_interval_secs == 0 {
            return Err(TuliproxError::ConfigBase("`interner_gc_interval_secs` must be > 0".to_string()));
        }
        if self.interner_gc_min_pool_size == 0 {
            return Err(TuliproxError::ConfigBase("`interner_gc_min_pool_size` must be > 0".to_string()));
        }

        if self.custom_stream_response_error_status == 0 {
            self.custom_stream_response_error_status = default_custom_stream_response_error_status();
        } else if !(400..=599).contains(&self.custom_stream_response_error_status) {
            return Err(TuliproxError::ConfigBase(format!(
                "`custom_stream_response_error_status` must be a 4xx or 5xx HTTP status, got {}",
                self.custom_stream_response_error_status
            )));
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

    pub(super) fn prepare_web(&mut self) -> Result<(), TuliproxError> {
        if let Some(web_ui_config) = self.web_ui.as_mut() {
            web_ui_config.prepare()?;
        }
        Ok(())
    }

    pub(super) fn prepare_hdhomerun(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        if let Some(hdhomerun) = &mut self.hdhomerun {
            if hdhomerun.enabled {
                hdhomerun.prepare(self.api.port, include_computed)?;
            }
        }
        Ok(())
    }

    pub(super) fn prepare_video_config(&mut self) -> Result<(), TuliproxError> {
        match &mut self.video {
            None => {
                self.video = Some(VideoConfigDto {
                    extensions: default_supported_video_extensions(),
                    web_search: None,
                    recording: None,
                });
            }
            Some(video) => match video.prepare() {
                Ok(()) => {}
                Err(err) => return Err(err),
            },
        }

        Ok(())
    }

    pub(super) fn prepare_metadata_update_config(&mut self) -> Result<(), TuliproxError> {
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
            if let Some(recording) = &video.recording {
                if let Some(episode_pattern) = &recording.episode_pattern {
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
            devices: hdhr.devices.iter().map(|d| d.name.clone()).collect::<Vec<String>>(),
        })
    }

    pub fn update_from_main_config(&mut self, main_config: &MainConfigDto) {
        // `recording` is intentionally NOT touched: this is the
        // simple-form save path; DVR is edited on its own form (see
        // the `MainConfigDto` comment).
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
        self.custom_stream_response_timeout_secs = main_config.custom_stream_response_timeout_secs;
        self.custom_stream_response_enabled = main_config.custom_stream_response_enabled;
        self.custom_stream_response_error_status = main_config.custom_stream_response_error_status;
        self.user_access_control = main_config.user_access_control;
        self.connect_timeout_secs = main_config.connect_timeout_secs;
        self.interner_gc_interval_secs = main_config.interner_gc_interval_secs;
        self.interner_gc_min_pool_size = main_config.interner_gc_min_pool_size;
        self.sleep_timer_mins = main_config.sleep_timer_mins;
        self.update_on_boot = main_config.update_on_boot;
        self.config_hot_reload = main_config.config_hot_reload;
        self.accept_insecure_ssl_certificates = main_config.accept_insecure_ssl_certificates;
    }

    pub fn is_geoip_enabled(&self) -> bool {
        self.reverse_proxy.as_ref().is_some_and(|r| r.geoip.as_ref().is_some_and(|g| g.enabled))
    }

    pub fn is_library_enabled(&self) -> bool { self.library.as_ref().is_some_and(|l| l.enabled) }

    pub fn is_stream_history_enabled(&self) -> bool {
        self.reverse_proxy.as_ref().and_then(|r| r.stream_history.as_ref()).is_some_and(|sh| sh.stream_history_enabled)
    }

    pub fn is_qos_aggregation_enabled(&self) -> bool {
        self.is_stream_history_enabled()
            && self.reverse_proxy.as_ref().and_then(|r| r.qos_aggregation.as_ref()).is_some_and(|qos| qos.enabled)
    }

    pub fn is_recording_enabled(&self) -> bool {
        self.video.as_ref().and_then(|v| v.recording.as_ref()).is_some_and(|r| r.enabled)
    }
}
