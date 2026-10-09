use crate::{
    defaults::{
        default_as_true, default_probe_delay_secs, default_probe_live_interval, default_resolve_background,
        default_resolve_delay_secs, default_xtream_live_stream_use_prefix, is_default_probe_delay_secs,
        is_default_probe_live_interval, is_default_resolve_delay_secs, is_false, is_true,
    },
    error::TuliproxError,
    foundation::{get_filter, Filter},
    model::{ConfigInputUpdateQualityDto, PatternTemplate, Prepare},
    utils::is_blank_optional_string,
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
        Ok(())
    }
}
