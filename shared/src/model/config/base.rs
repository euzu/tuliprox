use crate::{
    defaults::{
        default_as_true, default_connect_timeout_secs, default_custom_stream_response_error_status,
        default_custom_stream_response_path, default_default_user_agent, default_interner_gc_interval_secs,
        default_interner_gc_min_pool_size, default_main_backup_dir, default_main_mapping_path,
        default_main_storage_dir, default_main_template_path, default_main_user_config_dir,
        default_supported_video_extensions, is_blank_or_default_backup_dir,
        is_blank_or_default_custom_stream_response_path, is_blank_or_default_mapping_path,
        is_blank_or_default_storage_dir, is_blank_or_default_template_path, is_blank_or_default_user_config_dir,
        is_default_connect_timeout_secs, is_default_custom_stream_response_error_status,
        is_default_interner_gc_interval_secs, is_default_interner_gc_min_pool_size, is_false,
        is_none_or_empty_metadata_update, is_true, is_zero_u32, normalize_optional_config_file_path,
        normalize_optional_dir, DEFAULT_BACKUP_DIR, DEFAULT_CUSTOM_STREAM_RESPONSE_PATH, DEFAULT_DOWNLOAD_DIR,
        DEFAULT_STORAGE_DIR, DEFAULT_USER_CONFIG_DIR, MAPPING_FILE, TEMPLATE_FILE,
    },
    error::TuliproxError,
    model::{
        ConfigApiDto, HdHomeRunConfigDto, IpCheckConfigDto, LibraryConfigDto, LogConfigDto, MessagingConfigDto,
        MetadataUpdateConfigDto, ProxyConfigDto, RecordingConfigDto, ReverseProxyConfigDto, ScheduleConfigDto,
        VideoConfigDto, WebUiConfigDto,
    },
    utils::is_blank_optional_string,
};

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
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
    #[serde(
        default = "default_custom_stream_response_path",
        skip_serializing_if = "is_blank_or_default_custom_stream_response_path"
    )]
    pub custom_stream_response_path: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub custom_stream_response_timeout_secs: u32,
    /// When `true` (default), serve the configured
    /// MPEG-TS video. When `false`, the factories skip the video and the call
    /// sites return `custom_stream_response_error_status` instead of an
    /// infinite 200 OK loop. Use this behind a reverse proxy with
    /// `proxy_intercept_errors on;` to allow dead channels to be severed
    /// instead of pinning sockets open.
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub custom_stream_response_enabled: bool,
    /// HTTP status code returned when `custom_stream_response_enabled` is
    /// `false`. Must be a 4xx or 5xx code; the `prepare()` step rejects
    /// anything outside that range, and a configured `0` is silently
    /// clamped to the default `502`.
    #[serde(
        default = "default_custom_stream_response_error_status",
        skip_serializing_if = "is_default_custom_stream_response_error_status"
    )]
    pub custom_stream_response_error_status: u16,
    /// Canonical top-level DVR configuration. The split is intentional:
    /// download-side fields (`headers`, `extensions`, `episode_pattern`,
    /// `retry_backoff_*`, ...) stay on
    /// [`VideoDownloadConfigDto`] for the live/VOD download pipeline;
    /// this block owns the DVR-only fields (directory, timezone,
    /// retention, disk, quota, notifications). The legacy
    /// `VideoDownloadConfigDto::recording` carrier remains populated
    /// in memory while the compat shadow is consumed by readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingConfigDto>,
    /// Compat shadow of the legacy `video:` block. Populated on
    /// deserialize so frontend forms still see what they expect, but
    /// `skip_serializing` so canonical saves emit no `video:` key
    /// (`web_search` and the legacy nested `recording:` ride along).
    /// Remove in Task 11 once the frontend has moved off it.
    #[serde(default, skip_serializing)]
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
    #[serde(
        default = "default_interner_gc_interval_secs",
        skip_serializing_if = "is_default_interner_gc_interval_secs"
    )]
    pub interner_gc_interval_secs: u32,
    #[serde(
        default = "default_interner_gc_min_pool_size",
        skip_serializing_if = "is_default_interner_gc_min_pool_size"
    )]
    pub interner_gc_min_pool_size: u32,
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
            custom_stream_response_timeout_secs: 0,
            custom_stream_response_enabled: true,
            custom_stream_response_error_status: default_custom_stream_response_error_status(),
            recording: None,
            video: None,
            metadata_update: None,
            schedules: None,
            log: None,
            user_access_control: false,
            connect_timeout_secs: default_connect_timeout_secs(),
            interner_gc_interval_secs: default_interner_gc_interval_secs(),
            interner_gc_min_pool_size: default_interner_gc_min_pool_size(),
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

// Hand-written `Deserialize` to enforce the recording-unification contract:
// * both `recording:` AND `video.download.recording:` set with non-empty
//   content is rejected as ambiguous (the plan's fail-closed case).
// `Serialize` is derived: `#[serde(skip_serializing)]` on the `video`
// compat shadow keeps `web_search` and the legacy nested recording out
// of canonical output without hand-rolling a shadow struct.
impl<'de> serde::Deserialize<'de> for ConfigDto {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default, skip_serializing_if = "is_false")]
            process_parallel: bool,
            api: ConfigApiDto,
            #[serde(default, alias = "working_dir")]
            storage_dir: Option<String>,
            #[serde(default = "default_default_user_agent")]
            default_user_agent: Option<String>,
            #[serde(default)]
            backup_dir: Option<String>,
            #[serde(default)]
            user_config_dir: Option<String>,
            #[serde(default)]
            mapping_path: Option<String>,
            #[serde(default)]
            template_path: Option<String>,
            #[serde(default = "default_custom_stream_response_path")]
            custom_stream_response_path: Option<String>,
            #[serde(default)]
            custom_stream_response_timeout_secs: u32,
            #[serde(default = "default_as_true")]
            custom_stream_response_enabled: bool,
            #[serde(default = "default_custom_stream_response_error_status")]
            custom_stream_response_error_status: u16,
            #[serde(default)]
            recording: Option<RecordingConfigDto>,
            #[serde(default)]
            video: Option<VideoConfigDto>,
            #[serde(default)]
            metadata_update: Option<MetadataUpdateConfigDto>,
            #[serde(default)]
            schedules: Option<Vec<ScheduleConfigDto>>,
            #[serde(default)]
            log: Option<LogConfigDto>,
            #[serde(default)]
            user_access_control: bool,
            #[serde(default = "default_connect_timeout_secs")]
            connect_timeout_secs: u32,
            #[serde(default = "default_interner_gc_interval_secs")]
            interner_gc_interval_secs: u32,
            #[serde(default = "default_interner_gc_min_pool_size")]
            interner_gc_min_pool_size: u32,
            #[serde(default)]
            sleep_timer_mins: Option<u32>,
            #[serde(default)]
            update_on_boot: bool,
            #[serde(default)]
            config_hot_reload: bool,
            #[serde(default)]
            disk_based_processing: bool,
            #[serde(default)]
            accept_insecure_ssl_certificates: bool,
            #[serde(default)]
            web_ui: Option<WebUiConfigDto>,
            #[serde(default)]
            messaging: Option<MessagingConfigDto>,
            #[serde(default)]
            reverse_proxy: Option<ReverseProxyConfigDto>,
            #[serde(default)]
            hdhomerun: Option<HdHomeRunConfigDto>,
            #[serde(default)]
            proxy: Option<ProxyConfigDto>,
            #[serde(default)]
            ipcheck: Option<IpCheckConfigDto>,
            #[serde(default)]
            library: Option<LibraryConfigDto>,
        }

        let raw = Raw::deserialize(deserializer)?;

        // Ambiguity guard: if both canonical and legacy nested
        // recording blocks carry non-empty content, fail closed.
        // Empty legacy `recording: {}` is allowed (frontend
        // round-trips sometimes emit empty blocks).
        if raw.recording.is_some() && raw.video.is_some() {
            return Err(serde::de::Error::custom(
                "ambiguous canonical `recording:` and legacy `video:` blocks; remove the legacy block",
            ));
        }

        // Derive the canonical recording block:
        //  * explicit `recording:` wins (canonical or empty)
        //  * otherwise lift `video.download.recording` into canonical
        //  * otherwise leave None (the default form has no recording)
        let recording = raw.recording.or_else(|| raw.video.as_ref().map(migrate_legacy_video));

        Ok(Self {
            process_parallel: raw.process_parallel,
            api: raw.api,
            storage_dir: raw.storage_dir,
            default_user_agent: raw.default_user_agent,
            backup_dir: raw.backup_dir,
            user_config_dir: raw.user_config_dir,
            mapping_path: raw.mapping_path,
            template_path: raw.template_path,
            custom_stream_response_path: raw.custom_stream_response_path,
            custom_stream_response_timeout_secs: raw.custom_stream_response_timeout_secs,
            custom_stream_response_enabled: raw.custom_stream_response_enabled,
            custom_stream_response_error_status: raw.custom_stream_response_error_status,
            recording,
            video: raw.video,
            metadata_update: raw.metadata_update,
            schedules: raw.schedules,
            log: raw.log,
            user_access_control: raw.user_access_control,
            connect_timeout_secs: raw.connect_timeout_secs,
            interner_gc_interval_secs: raw.interner_gc_interval_secs,
            interner_gc_min_pool_size: raw.interner_gc_min_pool_size,
            sleep_timer_mins: raw.sleep_timer_mins,
            update_on_boot: raw.update_on_boot,
            config_hot_reload: raw.config_hot_reload,
            disk_based_processing: raw.disk_based_processing,
            accept_insecure_ssl_certificates: raw.accept_insecure_ssl_certificates,
            web_ui: raw.web_ui,
            messaging: raw.messaging,
            reverse_proxy: raw.reverse_proxy,
            hdhomerun: raw.hdhomerun,
            proxy: raw.proxy,
            ipcheck: raw.ipcheck,
            library: raw.library,
        })
    }
}

fn migrate_legacy_video(video: &VideoConfigDto) -> RecordingConfigDto {
    let Some(download) = video.download.as_ref() else {
        return RecordingConfigDto {
            enabled: false,
            extensions: video.extensions.clone(),
            ..RecordingConfigDto::default()
        };
    };

    let mut recording = download.recording.clone().unwrap_or_default();
    recording.headers.clone_from(&download.headers);
    recording.extensions.clone_from(&video.extensions);
    recording.organize_into_directories = download.organize_into_directories;
    recording.episode_pattern.clone_from(&download.episode_pattern);
    recording.priority = download.recording_priority;
    recording.reserve_slots_for_users = download.reserve_slots_for_users;
    recording.max_background_per_provider = download.max_background_per_provider;
    recording.retry_backoff_initial_secs = download.retry_backoff_initial_secs;
    recording.retry_backoff_multiplier = download.retry_backoff_multiplier;
    recording.retry_backoff_max_secs = download.retry_backoff_max_secs;
    recording.retry_backoff_jitter_percent = download.retry_backoff_jitter_percent;
    recording.retry_max_attempts = download.retry_max_attempts;
    if recording.directory.is_none() {
        let download_dir = download.directory.as_deref().unwrap_or(DEFAULT_DOWNLOAD_DIR);
        recording.directory = Some(crate::model::default_recording_directory(download_dir));
    }
    recording
}

// This MainConfigDto is a copy of ConfigDto simple fields for form editing.
// It has no other purpose than editing and saving the simple config values.
// `recording` is intentionally stripped here — the main-config form edits
// the simple scalar settings; DVR lives on a dedicated form/page (see
// `SchedulesConfigDto` for the parallel pattern). Add the field here only
// when the main form gains DVR editing.
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
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub custom_stream_response_timeout_secs: u32,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub custom_stream_response_enabled: bool,
    #[serde(
        default = "default_custom_stream_response_error_status",
        skip_serializing_if = "is_default_custom_stream_response_error_status"
    )]
    pub custom_stream_response_error_status: u16,
    #[serde(default, skip_serializing_if = "is_false")]
    pub user_access_control: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disk_based_processing: bool,
    #[serde(default = "default_connect_timeout_secs", skip_serializing_if = "is_default_connect_timeout_secs")]
    pub connect_timeout_secs: u32,
    #[serde(
        default = "default_interner_gc_interval_secs",
        skip_serializing_if = "is_default_interner_gc_interval_secs"
    )]
    pub interner_gc_interval_secs: u32,
    #[serde(
        default = "default_interner_gc_min_pool_size",
        skip_serializing_if = "is_default_interner_gc_min_pool_size"
    )]
    pub interner_gc_min_pool_size: u32,
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
            custom_stream_response_timeout_secs: 0,
            custom_stream_response_enabled: true,
            custom_stream_response_error_status: default_custom_stream_response_error_status(),
            user_access_control: false,
            connect_timeout_secs: default_connect_timeout_secs(),
            interner_gc_interval_secs: default_interner_gc_interval_secs(),
            interner_gc_min_pool_size: default_interner_gc_min_pool_size(),
            sleep_timer_mins: None,
            update_on_boot: false,
            config_hot_reload: false,
            accept_insecure_ssl_certificates: false,
        }
    }
}

impl From<&ConfigDto> for MainConfigDto {
    fn from(config: &ConfigDto) -> Self {
        // `recording` is intentionally NOT mirrored: the main-config form
        // owns simple scalar settings only (see the struct-level comment).
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
            custom_stream_response_timeout_secs: config.custom_stream_response_timeout_secs,
            custom_stream_response_enabled: config.custom_stream_response_enabled,
            custom_stream_response_error_status: config.custom_stream_response_error_status,
            user_access_control: config.user_access_control,
            connect_timeout_secs: config.connect_timeout_secs,
            interner_gc_interval_secs: config.interner_gc_interval_secs,
            interner_gc_min_pool_size: config.interner_gc_min_pool_size,
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
    // Clippy's method-path suggestion here names a private module and does not
    // compile; the closure is kept deliberately.
    #[allow(clippy::redundant_closure_for_method_calls)]
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

        // Also prepare the canonical top-level `recording` block. When
        // the user has only a `video.extensions` (no download, no nested
        // recording), the canonical recording is silently disabled —
        // there is no directory to record from. When `video.download`
        // is present but no nested recording, the canonical recording defaults
        // to enabled (the recording default).
        let download_dir = self.video.as_ref().and_then(|v| v.download.as_ref()).and_then(|d| d.directory.clone());
        let has_download = self.video.as_ref().and_then(|v| v.download.as_ref()).is_some();
        let fallback_dir = download_dir.as_deref().unwrap_or(DEFAULT_DOWNLOAD_DIR);
        if let Some(recording) = self.recording.as_mut() {
            super::video_download::prepare_recording_config(recording, fallback_dir)?;
        } else {
            let mut recording = if has_download {
                RecordingConfigDto::default()
            } else {
                RecordingConfigDto { enabled: false, ..RecordingConfigDto::default() }
            };
            super::video_download::prepare_recording_config(&mut recording, fallback_dir)?;
            if !recording.is_empty() || has_download {
                self.recording = Some(recording);
            }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defaults::{default_supported_video_extensions, CONFIG_PATH};
    use serde_json::json;

    #[test]
    fn default_uses_connect_timeout_default_value() {
        let cfg = ConfigDto::default();
        assert_eq!(cfg.connect_timeout_secs, default_connect_timeout_secs());
        assert_eq!(cfg.interner_gc_interval_secs, default_interner_gc_interval_secs());
        assert_eq!(cfg.interner_gc_min_pool_size, default_interner_gc_min_pool_size());
    }

    #[test]
    fn custom_video_stream_defaults_are_true_and_502() {
        let cfg = ConfigDto::default();
        assert!(cfg.custom_stream_response_enabled);
        assert_eq!(cfg.custom_stream_response_error_status, 502);
    }

    #[test]
    fn prepare_rejects_non_4xx_5xx_custom_stream_response_error_status() {
        for bad in [200u16, 100, 399, 600, 1000] {
            let mut cfg = ConfigDto { custom_stream_response_error_status: bad, ..ConfigDto::default() };
            let err = cfg.prepare(false).expect_err(&format!("status {bad} must be rejected"));
            let msg = format!("{err}");
            assert!(msg.contains("custom_stream_response_error_status"), "status {bad} msg: {msg}");
        }
    }

    #[test]
    fn prepare_accepts_4xx_and_5xx_custom_stream_response_error_status() {
        for ok in [400u16, 404, 500, 502, 503, 599] {
            let mut cfg = ConfigDto { custom_stream_response_error_status: ok, ..ConfigDto::default() };
            assert!(cfg.prepare(false).is_ok(), "status {ok} must be accepted");
        }
    }

    #[test]
    fn prepare_clamps_zero_custom_stream_response_error_status_to_default() {
        let mut cfg = ConfigDto { custom_stream_response_error_status: 0, ..ConfigDto::default() };
        cfg.prepare(false).expect("zero must be silently clamped, not rejected");
        assert_eq!(cfg.custom_stream_response_error_status, 502);
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
    fn serializing_omits_video_compat_shadow_even_when_populated() {
        // The compat `video:` shadow is no longer emitted on
        // serialization (the `skip_serializing` contract). Frontend
        // forms keep populating it programmatically; canonical saves
        // never re-emit it.
        let cfg = ConfigDto {
            video: Some(VideoConfigDto {
                extensions: default_supported_video_extensions(),
                download: None,
                web_search: Some("https://example.org?q={}".to_string()),
            }),
            ..ConfigDto::default()
        };

        let serialized = serde_json::to_string(&cfg).expect("config serialization should succeed");
        assert!(
            !serialized.contains("\"video\""),
            "video is a compat shadow and must not be re-emitted, got: {serialized}"
        );
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

    #[test]
    fn stream_history_defaults_to_disabled_and_safe() {
        let cfg = ConfigDto::default();

        assert!(cfg.reverse_proxy.is_none());
    }

    #[test]
    fn stream_history_deserializes_under_reverse_proxy() {
        let raw = r"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
reverse_proxy:
  rewrite_secret: 00112233445566778899aabbccddeeff
  stream_history:
    stream_history_enabled: true
    stream_history_batch_size: 64
    stream_history_retention_days: 14
    stream_history_directory: /var/lib/tuliprox/history
";

        let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config should deserialize");
        let reverse_proxy = cfg.reverse_proxy.expect("reverse_proxy should deserialize");
        let stream_history = reverse_proxy.stream_history.expect("stream_history should deserialize");

        assert!(stream_history.stream_history_enabled);
        assert_eq!(stream_history.stream_history_batch_size, 64);
        assert_eq!(stream_history.stream_history_retention_days, 14);
        assert_eq!(stream_history.stream_history_directory, "/var/lib/tuliprox/history");
    }

    #[test]
    fn stream_history_missing_values_keep_disabled_without_reverse_proxy() {
        let raw = r"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
";

        let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config should deserialize");

        assert!(cfg.reverse_proxy.is_none());
    }

    #[test]
    fn stream_history_defaults_to_none_when_reverse_proxy_present_but_stream_history_omitted() {
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
reverse_proxy:
  resource_rewrite_disabled: false
  rewrite_secret: "00112233445566778899aabbccddeeff"
"#;

        let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config should deserialize");

        assert!(cfg.reverse_proxy.is_some());
        assert!(cfg.reverse_proxy.as_ref().and_then(|rp| rp.stream_history.as_ref()).is_none());
    }

    // --- Recording schema-lift tests ---
    //
    // These tests pin the additive contract: `recording:` lives at the
    // top of the config going forward, while the legacy
    // `video.download.recording:` path keeps working until the compat
    // shadow is removed.
    // The custom deserializer in this module is responsible for
    // populating BOTH fields during the compatibility window.

    #[test]
    fn recording_top_level_deserializes_and_serializes_as_recording_only() {
        // Top-level `recording:` populates only the canonical field.
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
recording:
  enabled: true
  directory: /var/recordings
  timezone: Europe/Berlin
"#;

        let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("canonical recording deserializes");
        let recording = cfg.recording.as_ref().expect("recording should be Some");
        assert!(recording.enabled);
        assert_eq!(recording.directory.as_deref(), Some("/var/recordings"));
        assert_eq!(recording.timezone.as_deref(), Some("Europe/Berlin"));

        // Round-trip: serialize → parse, then assert actual field values.
        let serialized = serde_json::to_string(&cfg).expect("serialize");
        let roundtrip: ConfigDto = serde_json::from_str(&serialized).expect("round-trip parse");
        let rtp_recording = roundtrip.recording.as_ref().expect("round-trip recording preserved");
        assert!(rtp_recording.enabled, "round-trip recording.enabled must remain true");
        assert_eq!(rtp_recording.directory.as_deref(), Some("/var/recordings"));
        assert_eq!(rtp_recording.timezone.as_deref(), Some("Europe/Berlin"));
        assert!(!serialized.contains("\"video\""), "video compat shadow must not be re-emitted, got: {serialized}");
    }

    #[test]
    fn recording_legacy_video_download_nested_recording_migrates_without_losing_fields() {
        // Legacy nested `video.download.recording:` populates BOTH the
        // compat shadow and the canonical top-level field, without
        // dropping any DVR setting.
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
video:
  extensions: [".mp4"]
  download:
    directory: /data/downloads
    recording:
      enabled: true
      directory: /data/recordings
      timezone: Europe/London
      filename_template: "{channel}_{program_title}_{start_time}"
"#;

        let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("legacy config deserializes");

        // Compat shadow populated from the raw input.
        let legacy = cfg
            .video
            .as_ref()
            .and_then(|v| v.download.as_ref())
            .and_then(|d| d.recording.as_ref())
            .expect("legacy video.download.recording must round-trip");
        assert_eq!(legacy.directory.as_deref(), Some("/data/recordings"));
        assert_eq!(legacy.timezone.as_deref(), Some("Europe/London"));

        // Canonical top-level field carries the migrated values.
        let canonical = cfg.recording.as_ref().expect("canonical recording must be populated");
        assert!(canonical.enabled);
        assert_eq!(canonical.directory.as_deref(), Some("/data/recordings"));
        assert_eq!(canonical.timezone.as_deref(), Some("Europe/London"));
        assert_eq!(canonical.filename_template.as_deref(), Some("{channel}_{program_title}_{start_time}"));
    }

    #[test]
    fn recording_canonical_directory_wins_over_download_directory_default() {
        // `recording.directory` set explicitly takes precedence over
        // the `<download.directory>/recordings` default.
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
video:
  download:
    directory: /data/downloads
    recording:
      directory: /srv/recordings
"#;

        let mut cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config deserializes");
        cfg.prepare(false).expect("prepare should succeed");

        let canonical = cfg.recording.as_ref().expect("recording should be present");
        assert_eq!(canonical.directory.as_deref(), Some("/srv/recordings"));
    }

    #[test]
    fn recording_canonical_directory_defaults_to_download_directory_recordings() {
        // When `recording.directory` is absent, it falls back to
        // `<download.directory>/recordings`. This is the only place
        // the canonical field depends on the legacy `download`.
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
video:
  download:
    directory: /data/downloads
    recording: {}
"#;

        let mut cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config deserializes");
        cfg.prepare(false).expect("prepare should succeed");

        let canonical = cfg.recording.as_ref().expect("recording should be present");
        assert_eq!(canonical.directory.as_deref(), Some("/data/downloads/recordings"));
    }

    #[test]
    fn recording_only_extensions_yields_disabled_canonical_recording() {
        // `video.extensions` alone — no `download`, no nested
        // recording — produces a canonical `recording` with
        // `enabled: false`. There is no directory to record from.
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
video:
  extensions: [".ts"]
"#;

        let mut cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config deserializes");
        cfg.prepare(false).expect("prepare should succeed");

        let canonical = cfg.recording.as_ref().expect("recording should be present");
        assert!(!canonical.enabled, "extensions-only video must yield a disabled recording");
    }

    #[test]
    fn recording_download_without_nested_recording_preserves_enabled_default() {
        // `video.download` with no nested recording block keeps the
        // RecordingConfigDto default (`enabled: true`). The presence
        // of a download directory is a signal that DVR is in scope.
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
video:
  download:
    directory: /data/downloads
"#;

        let mut cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config deserializes");
        cfg.prepare(false).expect("prepare should succeed");

        let canonical = cfg.recording.as_ref().expect("recording should be present");
        assert!(canonical.enabled, "video.download without nested recording preserves the default");
    }

    #[test]
    fn recording_canonical_and_legacy_nested_recording_both_non_empty_is_ambiguous() {
        // Both `recording:` AND `video.download.recording:` set with
        // conflicting content must be rejected. We can't safely pick
        // one; this is the fail-closed contract from the plan.
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
recording:
  enabled: true
  directory: /var/recordings
video:
  extensions: [".mp4"]
  download:
    directory: /data/downloads
    recording:
      enabled: true
      directory: /data/recordings
"#;

        let err = serde_saphyr::from_str::<ConfigDto>(raw)
            .expect_err("canonical + legacy nested recording must be rejected as ambiguous");
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous") || msg.contains("recording"), "unexpected error: {msg}");
    }

    #[test]
    fn recording_web_search_is_accepted_from_legacy_input_but_never_serialized() {
        // `video.web_search` round-trips in-memory (frontend uses it)
        // but the canonical config never re-emits it.
        let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
video:
  extensions: [".mp4"]
  web_search: "https://example.org/?q={query}"
"#;

        let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config deserializes");
        let web_search = cfg.video.as_ref().and_then(|v| v.web_search.as_ref()).expect("web_search present");
        assert_eq!(web_search, "https://example.org/?q={query}");

        let serialized = serde_json::to_string(&cfg).expect("serialize");
        assert!(!serialized.contains("web_search"), "web_search must never be serialized, got: {serialized}");
    }

    #[test]
    fn recording_round_trips_full_canonical_block_via_canonical_field() {
        // All four DVR sub-blocks populated at once — the canonical
        // path must preserve retention/disk/quota/notifications
        // bit-for-bit across serialize → deserialize.
        use crate::model::{
            RecordingDiskConfigDto, RecordingNotificationConfigDto, RecordingQuotaConfigDto,
            RecordingRetentionConfigDto,
        };
        use std::collections::HashMap;

        let original = RecordingConfigDto {
            enabled: true,
            directory: Some("/srv/recordings".to_string()),
            timezone: Some("Asia/Tokyo".to_string()),
            filename_template: Some("{channel}_{program_title}_{start_time}".to_string()),
            default_pre_roll_secs: Some(30),
            max_pre_roll_secs: 600,
            default_post_roll_secs: Some(60),
            max_post_roll_secs: 1200,
            retention: Some(RecordingRetentionConfigDto {
                keep_last_per_channel: Some(7),
                delete_after_days: Some(14),
                sweep_interval_secs: 1800,
            }),
            disk: Some(RecordingDiskConfigDto {
                high_water_percent: Some(90),
                low_water_percent: Some(70),
                cleanup_interval_secs: Some(900),
                safety_bytes: Some(2 * 1024 * 1024 * 1024),
            }),
            quota: Some(RecordingQuotaConfigDto {
                default_private_bytes: Some(50 * 1024 * 1024 * 1024),
                per_user_bytes: HashMap::from([("alice".to_string(), 100 * 1024 * 1024 * 1024)]),
                shared_bytes: Some(200 * 1024 * 1024 * 1024),
            }),
            notifications: Some(RecordingNotificationConfigDto {
                outbox_buffer: 2048,
                max_attempts: 8,
                backoff_initial_secs: 7,
                backoff_max_secs: 1200,
            }),
            ..RecordingConfigDto::default()
        };

        let mut cfg = ConfigDto { recording: Some(original.clone()), ..ConfigDto::default() };
        cfg.prepare(false).expect("prepare should succeed");

        let serialized = serde_json::to_string(&cfg).expect("serialize");
        let roundtrip: ConfigDto = serde_json::from_str(&serialized).expect("round-trip parse");

        let rtp = roundtrip.recording.as_ref().expect("recording preserved");
        assert_eq!(rtp.directory.as_deref(), Some("/srv/recordings"));
        assert_eq!(rtp.timezone.as_deref(), Some("Asia/Tokyo"));
        let retention = rtp.retention.as_ref().expect("retention preserved");
        assert_eq!(retention.keep_last_per_channel, Some(7));
        assert_eq!(retention.delete_after_days, Some(14));
        let disk = rtp.disk.as_ref().expect("disk preserved");
        assert_eq!(disk.high_water_percent, Some(90));
        assert_eq!(disk.low_water_percent, Some(70));
        let quota = rtp.quota.as_ref().expect("quota preserved");
        assert_eq!(quota.shared_bytes, Some(200 * 1024 * 1024 * 1024));
        let notifications = rtp.notifications.as_ref().expect("notifications preserved");
        assert_eq!(notifications.outbox_buffer, 2048);
    }

    #[test]
    fn config_dto_default_recording_is_none_before_prepare_and_some_disabled_after() {
        // Pin: `ConfigDto::default()` has no canonical recording
        // block (frontend reads `None` until the operator populates
        // anything). After `prepare(false)` with no `video.download`,
        // the canonical block is created in disabled form — there is
        // nothing to record from.
        let mut cfg = ConfigDto::default();
        assert!(cfg.recording.is_none(), "default ConfigDto has no recording block");
        cfg.prepare(false).expect("prepare on default should succeed");
        let recording = cfg.recording.as_ref().expect("prepare creates a disabled canonical recording");
        assert!(!recording.enabled, "no download directory implies recording is disabled");
    }

    #[test]
    fn is_none_or_empty_video_deletion_does_not_change_video_serialization() {
        // The dead `is_none_or_empty_video` was the only thing
        // referenced by the old `skip_serializing_if` on `video:`.
        // After its removal the field is `skip_serializing`. This
        // test pins that the behavior — never emitting `video:` — is
        // preserved regardless of which underlying predicate the
        // derive sees.
        let cfg = ConfigDto {
            video: Some(VideoConfigDto {
                extensions: vec![".mp4".to_string()],
                download: None,
                web_search: Some("https://example.org/?q={query}".to_string()),
            }),
            ..ConfigDto::default()
        };
        let serialized = serde_json::to_string(&cfg).expect("serialize");
        assert!(!serialized.contains("\"video\""), "video compat shadow is never emitted: {serialized}");
    }
}
