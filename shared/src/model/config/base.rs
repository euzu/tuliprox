use crate::{
    defaults::{
        default_as_true, default_connect_timeout_secs, default_custom_stream_response_error_status,
        default_custom_stream_response_path, default_default_user_agent, default_event_channel_capacity,
        default_interner_gc_interval_secs, default_interner_gc_min_pool_size, is_blank_or_default_backup_dir,
        is_blank_or_default_custom_stream_response_path, is_blank_or_default_mapping_path,
        is_blank_or_default_storage_dir, is_blank_or_default_template_path, is_blank_or_default_user_config_dir,
        is_default_connect_timeout_secs, is_default_custom_stream_response_error_status,
        is_default_event_channel_capacity, is_default_interner_gc_interval_secs, is_default_interner_gc_min_pool_size,
        is_false, is_none_or_empty_metadata_update, is_true, is_zero_u32,
    },
    model::{
        ConfigApiDto, HdHomeRunConfigDto, IpCheckConfigDto, LibraryConfigDto, LogConfigDto, MessagingConfigDto,
        MetadataUpdateConfigDto, ProxyConfigDto, ReverseProxyConfigDto, ScheduleConfigDto, VideoConfigDto,
        WebUiConfigDto,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(default = "default_event_channel_capacity", skip_serializing_if = "is_default_event_channel_capacity")]
    pub event_channel_capacity: u32,
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
            video: None,
            metadata_update: None,
            schedules: None,
            log: None,
            user_access_control: false,
            connect_timeout_secs: default_connect_timeout_secs(),
            interner_gc_interval_secs: default_interner_gc_interval_secs(),
            event_channel_capacity: default_event_channel_capacity(),
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

// Hand-written deserialization keeps the top-level schema strict while
// preserving the project-specific defaults used by the configuration form.
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
            #[serde(default = "default_event_channel_capacity")]
            event_channel_capacity: u32,
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
            video: raw.video,
            metadata_update: raw.metadata_update,
            schedules: raw.schedules,
            log: raw.log,
            user_access_control: raw.user_access_control,
            connect_timeout_secs: raw.connect_timeout_secs,
            event_channel_capacity: raw.event_channel_capacity,
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

#[cfg(test)]
mod tests;

mod prepare;
mod views;
pub use views::{HdHomeRunDeviceOverview, MainConfigDto, SchedulesConfigDto};
