use crate::{
    defaults::{
        default_as_true, default_probe_delay_secs, default_probe_live_interval, default_resolve_background,
        default_resolve_delay_secs, default_xtream_live_stream_use_prefix, is_default_probe_delay_secs,
        is_default_probe_live_interval, is_default_resolve_delay_secs, is_false, is_true,
    },
    error::TuliproxError,
    foundation::{get_filter, Filter},
    model::{ConfigInputUpdateQualityDto, PatternTemplate, Prepare},
    utils::{is_blank_optional_string, normalize_container_extension},
};

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Eq,
    strum_macros::Display,
    strum_macros::EnumString,
    strum_macros::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum FlussonicHlsCatchup {
    #[default]
    Native,
    BoundedArchive,
}

impl FlussonicHlsCatchup {
    pub fn is_native(value: &Self) -> bool { matches!(value, Self::Native) }
}

pub const fn default_flussonic_hls_catchup_max_duration_secs() -> u32 { 4 * 60 * 60 }

fn is_default_flussonic_hls_catchup_max_duration_secs(value: &u32) -> bool {
    *value == default_flussonic_hls_catchup_max_duration_secs()
}

pub const DEFAULT_CONTAINER_EXTENSION_SAMPLING_MAX_CALLS: u16 = 20;
pub const MAX_CONTAINER_EXTENSION_SAMPLING_CALLS: u16 = 200;
pub const DEFAULT_CONTAINER_EXTENSION_SAMPLING_MIN_INTERVAL_SECS: u16 = 3;
pub const DEFAULT_CONTAINER_EXTENSION_SAMPLING_STOP_AFTER_REFUSALS: u8 = 2;

const fn default_container_extension_sampling_max_calls() -> u16 { DEFAULT_CONTAINER_EXTENSION_SAMPLING_MAX_CALLS }

fn is_default_container_extension_sampling_max_calls(value: &u16) -> bool {
    *value == DEFAULT_CONTAINER_EXTENSION_SAMPLING_MAX_CALLS
}

const fn default_container_extension_sampling_min_interval_secs() -> u16 {
    DEFAULT_CONTAINER_EXTENSION_SAMPLING_MIN_INTERVAL_SECS
}

fn is_default_container_extension_sampling_min_interval_secs(value: &u16) -> bool {
    *value == DEFAULT_CONTAINER_EXTENSION_SAMPLING_MIN_INTERVAL_SECS
}

const fn default_container_extension_sampling_stop_after_refusals() -> u8 {
    DEFAULT_CONTAINER_EXTENSION_SAMPLING_STOP_AFTER_REFUSALS
}

fn is_default_container_extension_sampling_stop_after_refusals(value: &u8) -> bool {
    *value == DEFAULT_CONTAINER_EXTENSION_SAMPLING_STOP_AFTER_REFUSALS
}

/// Opt-in, budgeted sampling of the provider's `create_link` answer for Stalker VOD items.
///
/// A Stalker portal states a movie's container only in the playback answer, so learning it costs a
/// portal request. The limits keep that cost bounded: every playlist update samples at most
/// `max_calls_per_update` items, waits `min_interval_secs` between calls, and stops the run after
/// `stop_after_refusals` consecutive auth or rate refusals (401/403/429).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContainerExtensionSamplingDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(
        default = "default_container_extension_sampling_max_calls",
        skip_serializing_if = "is_default_container_extension_sampling_max_calls"
    )]
    pub max_calls_per_update: u16,
    #[serde(
        default = "default_container_extension_sampling_min_interval_secs",
        skip_serializing_if = "is_default_container_extension_sampling_min_interval_secs"
    )]
    pub min_interval_secs: u16,
    #[serde(
        default = "default_container_extension_sampling_stop_after_refusals",
        skip_serializing_if = "is_default_container_extension_sampling_stop_after_refusals"
    )]
    pub stop_after_refusals: u8,
}

impl Default for ContainerExtensionSamplingDto {
    fn default() -> Self {
        Self {
            enabled: false,
            max_calls_per_update: DEFAULT_CONTAINER_EXTENSION_SAMPLING_MAX_CALLS,
            min_interval_secs: DEFAULT_CONTAINER_EXTENSION_SAMPLING_MIN_INTERVAL_SECS,
            stop_after_refusals: DEFAULT_CONTAINER_EXTENSION_SAMPLING_STOP_AFTER_REFUSALS,
        }
    }
}

impl ContainerExtensionSamplingDto {
    /// `true` when the block holds nothing but defaults, so it is omitted from the document.
    pub fn is_disabled(&self) -> bool { *self == Self::default() }

    pub fn clean(&mut self) { *self = Self::default(); }

    fn validate(&self) -> Result<(), TuliproxError> {
        if !(1..=MAX_CONTAINER_EXTENSION_SAMPLING_CALLS).contains(&self.max_calls_per_update) {
            return Err(TuliproxError::ConfigInput(format!(
                "options.container_extension_sampling.max_calls_per_update must be between 1 and \
                 {MAX_CONTAINER_EXTENSION_SAMPLING_CALLS}"
            )));
        }
        if self.min_interval_secs == 0 {
            return Err(TuliproxError::ConfigInput(
                "options.container_extension_sampling.min_interval_secs must be at least 1".to_string(),
            ));
        }
        if self.stop_after_refusals == 0 {
            return Err(TuliproxError::ConfigInput(
                "options.container_extension_sampling.stop_after_refusals must be at least 1".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigInputOptionsDto {
    #[serde(default, alias = "xtream_skip_live", alias = "stalker_skip_live", skip_serializing_if = "is_false")]
    pub skip_live: bool,
    #[serde(default, alias = "xtream_skip_vod", alias = "stalker_skip_vod", skip_serializing_if = "is_false")]
    pub skip_vod: bool,
    #[serde(default, alias = "xtream_skip_series", alias = "stalker_skip_series", skip_serializing_if = "is_false")]
    pub skip_series: bool,
    #[serde(default, skip_serializing_if = "ConfigInputUpdateQualityDto::is_disabled")]
    pub update_quality: ConfigInputUpdateQualityDto,
    #[serde(default = "default_xtream_live_stream_use_prefix", skip_serializing_if = "is_true")]
    pub xtream_live_stream_use_prefix: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub xtream_live_stream_without_extension: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_hls_streaming: bool,
    #[serde(default, skip_serializing_if = "FlussonicHlsCatchup::is_native")]
    pub flussonic_hls_catchup: FlussonicHlsCatchup,
    #[serde(
        default = "default_flussonic_hls_catchup_max_duration_secs",
        skip_serializing_if = "is_default_flussonic_hls_catchup_max_duration_secs"
    )]
    pub flussonic_hls_catchup_max_duration_secs: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub user_agent_stream_index: bool,
    /// Labels fragmented-MP4 objects below Flussonic `tracks-a<N>` paths as `audio/mp4` when the
    /// provider sends no specific Content-Type.
    #[serde(default, skip_serializing_if = "is_false")]
    pub flussonic_hls_audio_tracks: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_tmdb: bool,
    #[serde(default = "default_resolve_background", skip_serializing_if = "is_true")]
    pub resolve_background: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_series: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolve_vod: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub probe_series: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub probe_vod: bool,
    #[serde(default = "default_resolve_delay_secs", skip_serializing_if = "is_default_resolve_delay_secs")]
    pub resolve_delay: u16,
    #[serde(default = "default_probe_delay_secs", skip_serializing_if = "is_default_probe_delay_secs")]
    pub probe_delay: u16,
    #[serde(default, alias = "resolve_live", skip_serializing_if = "is_false")]
    pub probe_live: bool,
    #[serde(
        default = "default_probe_live_interval",
        alias = "resolve_live_interval_hours",
        skip_serializing_if = "is_default_probe_live_interval"
    )]
    pub probe_live_interval_hours: u32,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub resolve_filter: Option<String>,
    #[serde(skip)]
    pub t_resolve_filter: Option<Filter>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub probe_filter: Option<String>,
    #[serde(skip)]
    pub t_probe_filter: Option<Filter>,
    /// Bulk-fetch EPG for live channels during playlist processing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stalker_bulk_epg: bool,
    /// Container extension published for movies and series episodes when neither the provider nor the
    /// stream URL states one. Unset means the built-in default `mp4`. Applied at delivery only: it
    /// fills a blank field in the Xtream response and never overrides a provider value.
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub default_container_extension: Option<String>,
    /// Opt-in sampling of the provider's playback answer for Stalker VOD container extensions.
    #[serde(default, skip_serializing_if = "ContainerExtensionSamplingDto::is_disabled")]
    pub container_extension_sampling: ContainerExtensionSamplingDto,
}

impl Default for ConfigInputOptionsDto {
    fn default() -> Self {
        ConfigInputOptionsDto {
            skip_live: false,
            skip_vod: false,
            skip_series: false,
            update_quality: ConfigInputUpdateQualityDto::default(),
            xtream_live_stream_use_prefix: default_xtream_live_stream_use_prefix(),
            xtream_live_stream_without_extension: false,
            disable_hls_streaming: false,
            flussonic_hls_catchup: FlussonicHlsCatchup::Native,
            flussonic_hls_catchup_max_duration_secs: default_flussonic_hls_catchup_max_duration_secs(),
            user_agent_stream_index: false,
            flussonic_hls_audio_tracks: false,
            resolve_tmdb: false,
            resolve_background: default_resolve_background(),
            resolve_series: false,
            resolve_vod: false,
            probe_series: false,
            probe_vod: false,
            resolve_delay: default_resolve_delay_secs(),
            probe_delay: default_probe_delay_secs(),
            probe_live: false,
            probe_live_interval_hours: default_probe_live_interval(),
            resolve_filter: None,
            t_resolve_filter: None,
            probe_filter: None,
            t_probe_filter: None,
            stalker_bulk_epg: false,
            default_container_extension: None,
            container_extension_sampling: ContainerExtensionSamplingDto::default(),
        }
    }
}

impl ConfigInputOptionsDto {
    pub fn is_empty(&self) -> bool {
        !self.skip_live
            && !self.skip_vod
            && !self.skip_series
            && self.update_quality.is_empty()
            && self.xtream_live_stream_use_prefix
            && !self.xtream_live_stream_without_extension
            && !self.disable_hls_streaming
            && FlussonicHlsCatchup::is_native(&self.flussonic_hls_catchup)
            && is_default_flussonic_hls_catchup_max_duration_secs(&self.flussonic_hls_catchup_max_duration_secs)
            && !self.user_agent_stream_index
            && !self.flussonic_hls_audio_tracks
            && !self.resolve_tmdb
            && self.resolve_background
            && !self.resolve_series
            && !self.resolve_vod
            && !self.probe_series
            && !self.probe_vod
            && is_default_resolve_delay_secs(&self.resolve_delay)
            && is_default_probe_delay_secs(&self.probe_delay)
            && !self.probe_live
            && is_default_probe_live_interval(&self.probe_live_interval_hours)
            && self.resolve_filter.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.probe_filter.as_ref().is_none_or(|s| s.trim().is_empty())
            && !self.stalker_bulk_epg
            && self.default_container_extension.as_ref().is_none_or(|s| s.trim().is_empty())
            && self.container_extension_sampling.is_disabled()
    }

    pub fn clean(&mut self) {
        self.skip_live = false;
        self.skip_vod = false;
        self.skip_series = false;
        self.update_quality.clean();
        self.xtream_live_stream_use_prefix = default_as_true();
        self.xtream_live_stream_without_extension = false;
        self.disable_hls_streaming = false;
        self.flussonic_hls_catchup = FlussonicHlsCatchup::Native;
        self.flussonic_hls_catchup_max_duration_secs = default_flussonic_hls_catchup_max_duration_secs();
        self.user_agent_stream_index = false;
        self.flussonic_hls_audio_tracks = false;
        self.resolve_tmdb = false;
        self.resolve_background = default_as_true();
        self.resolve_series = false;
        self.resolve_vod = false;
        self.probe_series = false;
        self.probe_vod = false;
        self.resolve_delay = default_resolve_delay_secs();
        self.probe_delay = default_probe_delay_secs();
        self.probe_live = false;
        self.probe_live_interval_hours = default_probe_live_interval();
        self.resolve_filter = None;
        self.t_resolve_filter = None;
        self.probe_filter = None;
        self.t_probe_filter = None;
        self.stalker_bulk_epg = false;
        self.default_container_extension = None;
        self.container_extension_sampling.clean();
    }
}

impl Prepare for ConfigInputOptionsDto {
    type Ctx<'a> = Option<&'a [PatternTemplate]>;

    fn prepare(&mut self, templates: Self::Ctx<'_>) -> Result<(), TuliproxError> {
        self.update_quality.prepare(())?;
        if !(1..=7 * 24 * 60 * 60).contains(&self.flussonic_hls_catchup_max_duration_secs) {
            return Err(TuliproxError::ConfigInput(
                "flussonic_hls_catchup_max_duration_secs must be between 1 and 604800 seconds".to_string(),
            ));
        }
        if let Some(raw_filter) = &self.resolve_filter {
            self.t_resolve_filter = Some(get_filter(raw_filter, templates)?);
        }
        if let Some(raw_filter) = &self.probe_filter {
            self.t_probe_filter = Some(get_filter(raw_filter, templates)?);
        }
        self.container_extension_sampling.validate()?;
        if let Some(raw_extension) = self.default_container_extension.as_deref() {
            if raw_extension.trim().is_empty() {
                self.default_container_extension = None;
            } else {
                let Some(normalized) = normalize_container_extension(raw_extension) else {
                    return Err(TuliproxError::ConfigInput(format!(
                        "options.default_container_extension '{raw_extension}' is not a container extension; use a short \
                         alphanumeric token such as mp4 or mkv"
                    )));
                };
                self.default_container_extension = Some(normalized.into_owned());
            }
        }
        Ok(())
    }
}
