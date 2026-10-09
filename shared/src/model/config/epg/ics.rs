use super::prepare::is_absolute_local_path;
use crate::{
    defaults::{
        default_ics_dummy_block_hours, default_ics_dummy_days_future, default_ics_dummy_days_past,
        default_ics_dummy_min_gap_minutes, default_ics_dummy_title, default_ics_event_description,
        default_ics_event_title, default_ics_max_decompressed_bytes, default_ics_max_download_bytes,
        default_ics_max_events, default_ics_timezone, is_default_ics_dummy_block_hours,
        is_default_ics_dummy_days_future, is_default_ics_dummy_days_past, is_default_ics_dummy_min_gap_minutes,
        is_default_ics_dummy_title, is_default_ics_event_description, is_default_ics_event_title,
        is_default_ics_max_decompressed_bytes, is_default_ics_max_download_bytes, is_default_ics_max_events,
        is_default_ics_timezone, is_false, MAX_ICS_DAYS_FUTURE, MAX_ICS_DAYS_PAST,
        MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT, MAX_ICS_DESCRIPTION_LENGTH, MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT,
        MAX_ICS_EVENTS_HARD_LIMIT, MAX_ICS_SUMMARY_LENGTH,
    },
    error::TuliproxError,
    utils::sanitize_sensitive_info,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IcsEpgSourceConfigDto {
    #[serde(default = "default_ics_timezone", skip_serializing_if = "is_default_ics_timezone")]
    pub timezone: String,
    #[serde(default, skip_serializing_if = "IcsEventMappingDto::is_default")]
    pub event: IcsEventMappingDto,
    #[serde(default, skip_serializing_if = "IcsDummyConfigDto::is_default")]
    pub dummy: IcsDummyConfigDto,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_cancelled: bool,
    #[serde(default = "default_ics_max_events", skip_serializing_if = "is_default_ics_max_events")]
    pub max_events: usize,
    #[serde(default = "default_ics_max_download_bytes", skip_serializing_if = "is_default_ics_max_download_bytes")]
    pub max_download_bytes: u64,
    #[serde(
        default = "default_ics_max_decompressed_bytes",
        skip_serializing_if = "is_default_ics_max_decompressed_bytes"
    )]
    pub max_decompressed_bytes: usize,
}

impl Default for IcsEpgSourceConfigDto {
    fn default() -> Self {
        Self {
            timezone: default_ics_timezone(),
            event: IcsEventMappingDto::default(),
            dummy: IcsDummyConfigDto::default(),
            include_cancelled: false,
            max_events: default_ics_max_events(),
            max_download_bytes: default_ics_max_download_bytes(),
            max_decompressed_bytes: default_ics_max_decompressed_bytes(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IcsEventMappingDto {
    #[serde(default = "default_ics_event_title", skip_serializing_if = "is_default_ics_event_title")]
    pub title: String,
    #[serde(default = "default_ics_event_description", skip_serializing_if = "is_default_ics_event_description")]
    pub description: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_location: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_categories: bool,
}

impl Default for IcsEventMappingDto {
    fn default() -> Self {
        Self {
            title: default_ics_event_title(),
            description: default_ics_event_description(),
            include_location: false,
            include_categories: false,
        }
    }
}

impl IcsEventMappingDto {
    pub fn is_default(value: &Self) -> bool { value == &Self::default() }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IcsDummyConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(default = "default_ics_dummy_title", skip_serializing_if = "is_default_ics_dummy_title")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default = "default_ics_dummy_days_past", skip_serializing_if = "is_default_ics_dummy_days_past")]
    pub days_past: u16,
    #[serde(default = "default_ics_dummy_days_future", skip_serializing_if = "is_default_ics_dummy_days_future")]
    pub days_future: u16,
    #[serde(default = "default_ics_dummy_block_hours", skip_serializing_if = "is_default_ics_dummy_block_hours")]
    pub block_hours: u8,
    #[serde(
        default = "default_ics_dummy_min_gap_minutes",
        skip_serializing_if = "is_default_ics_dummy_min_gap_minutes"
    )]
    pub min_gap_minutes: u16,
}

impl Default for IcsDummyConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            title: default_ics_dummy_title(),
            description: String::new(),
            days_past: default_ics_dummy_days_past(),
            days_future: default_ics_dummy_days_future(),
            block_hours: default_ics_dummy_block_hours(),
            min_gap_minutes: default_ics_dummy_min_gap_minutes(),
        }
    }
}

impl IcsDummyConfigDto {
    pub fn is_default(value: &Self) -> bool { value == &Self::default() }
}

pub(super) fn validate_ics_config(config: &IcsEpgSourceConfigDto) -> Result<(), TuliproxError> {
    validate_ics_timezone(&config.timezone)?;
    let block_hours = config.dummy.block_hours;
    if block_hours == 0 || block_hours > 24 || 24 % block_hours != 0 {
        return Err(TuliproxError::ConfigEpg(format!(
            "ics.dummy.block_hours must divide 24 evenly, got {block_hours}"
        )));
    }
    if config.max_events == 0 {
        return Err(TuliproxError::ConfigEpg("ics.max_events must be greater than 0".to_string()));
    }
    if config.max_events > MAX_ICS_EVENTS_HARD_LIMIT {
        return Err(TuliproxError::ConfigEpg(format!("ics.max_events must not exceed {MAX_ICS_EVENTS_HARD_LIMIT}")));
    }
    if config.max_download_bytes == 0 {
        return Err(TuliproxError::ConfigEpg("ics.max_download_bytes must be greater than 0".to_string()));
    }
    if config.max_download_bytes > MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT {
        return Err(TuliproxError::ConfigEpg(format!(
            "ics.max_download_bytes must not exceed {MAX_ICS_DOWNLOAD_BYTES_HARD_LIMIT}"
        )));
    }
    if config.max_decompressed_bytes == 0 {
        return Err(TuliproxError::ConfigEpg("ics.max_decompressed_bytes must be greater than 0".to_string()));
    }
    if config.max_decompressed_bytes > MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT {
        return Err(TuliproxError::ConfigEpg(format!(
            "ics.max_decompressed_bytes must not exceed {MAX_ICS_DECOMPRESSED_BYTES_HARD_LIMIT}"
        )));
    }
    if config.dummy.days_past > MAX_ICS_DAYS_PAST {
        return Err(TuliproxError::ConfigEpg(format!("ics.dummy.days_past must not exceed {MAX_ICS_DAYS_PAST}")));
    }
    if config.dummy.days_future > MAX_ICS_DAYS_FUTURE {
        return Err(TuliproxError::ConfigEpg(format!("ics.dummy.days_future must not exceed {MAX_ICS_DAYS_FUTURE}")));
    }
    validate_text_limit("ics.event.title", &config.event.title, MAX_ICS_SUMMARY_LENGTH)?;
    validate_text_limit("ics.event.description", &config.event.description, MAX_ICS_DESCRIPTION_LENGTH)?;
    validate_text_limit("ics.dummy.title", &config.dummy.title, MAX_ICS_SUMMARY_LENGTH)?;
    validate_text_limit("ics.dummy.description", &config.dummy.description, MAX_ICS_DESCRIPTION_LENGTH)?;
    Ok(())
}

fn validate_ics_timezone(timezone: &str) -> Result<(), TuliproxError> {
    if timezone.is_empty() {
        return Err(TuliproxError::ConfigEpg("ics.timezone must not be empty".to_string()));
    }
    timezone
        .parse::<chrono_tz::Tz>()
        .map(|_| ())
        .map_err(|_| TuliproxError::ConfigEpg(format!("ics.timezone '{timezone}' is not a valid IANA timezone")))
}

fn validate_text_limit(field: &str, value: &str, max_len: usize) -> Result<(), TuliproxError> {
    if value.len() > max_len {
        return Err(TuliproxError::ConfigEpg(format!("{field} must not exceed {max_len} bytes")));
    }
    Ok(())
}

pub(super) fn validate_ics_url_scheme(url: &str) -> Result<(), TuliproxError> {
    if is_absolute_local_path(url) {
        return Ok(());
    }

    let Ok(parsed) = url::Url::parse(url) else {
        return Ok(());
    };

    match parsed.scheme() {
        "https" | "http" | "file" | "provider" => Ok(()),
        scheme => Err(TuliproxError::ConfigEpg(format!(
            "Unsupported ICS url scheme '{scheme}' for {}",
            sanitize_sensitive_info(url)
        ))),
    }
}

pub(super) fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let candidate = value.get(..prefix.len())?;
    if !candidate.eq_ignore_ascii_case(prefix) {
        return None;
    }
    value.get(prefix.len()..)
}
