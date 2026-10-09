use super::ConfigDto;
use crate::{
    defaults::{
        default_as_true, default_connect_timeout_secs, default_custom_stream_response_error_status,
        default_default_user_agent, default_event_channel_capacity, default_interner_gc_interval_secs,
        default_interner_gc_min_pool_size, default_main_backup_dir, default_main_mapping_path,
        default_main_storage_dir, default_main_template_path, default_main_user_config_dir,
        is_blank_or_default_backup_dir, is_blank_or_default_mapping_path, is_blank_or_default_storage_dir,
        is_blank_or_default_template_path, is_blank_or_default_user_config_dir, is_default_connect_timeout_secs,
        is_default_custom_stream_response_error_status, is_default_event_channel_capacity,
        is_default_interner_gc_interval_secs, is_default_interner_gc_min_pool_size, is_false, is_true, is_zero_u32,
    },
    model::ScheduleConfigDto,
    utils::is_blank_optional_string,
};

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
    #[serde(default = "default_event_channel_capacity", skip_serializing_if = "is_default_event_channel_capacity")]
    pub event_channel_capacity: u32,
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
            event_channel_capacity: default_event_channel_capacity(),
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
            event_channel_capacity: config.event_channel_capacity,
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
