use super::{MetadataUpdateConfigDto, DEFAULT_FFPROBE_TIMEOUT_SECS, MIN_DURATION_SECS};
use crate::{
    defaults::{
        default_metadata_ffprobe_analyze_duration, default_metadata_ffprobe_live_analyze_duration,
        default_metadata_ffprobe_live_probe_size, default_metadata_ffprobe_probe_size, default_metadata_tmdb_cooldown,
        default_tmdb_api_key, default_tmdb_cache_duration_days, default_tmdb_language, default_tmdb_match_threshold,
        default_tmdb_rate_limit_ms, is_default_metadata_ffprobe_analyze_duration,
        is_default_metadata_ffprobe_live_analyze_duration, is_default_metadata_ffprobe_live_probe_size,
        is_default_metadata_ffprobe_probe_size, is_default_metadata_tmdb_cooldown, is_default_tmdb_cache_duration_days,
        is_default_tmdb_language, is_default_tmdb_match_threshold, is_default_tmdb_rate_limit_ms, is_false,
        is_tmdb_default_api_key, TMDB_API_KEY,
    },
    error::TuliproxError,
    model::ByteSize,
    utils::deserialize_as_string,
};

fn default_ffprobe_timeout_secs() -> Option<u64> { Some(DEFAULT_FFPROBE_TIMEOUT_SECS) }

fn is_default_ffprobe_timeout(timeout: &Option<u64>) -> bool {
    timeout.is_none_or(|value| value == DEFAULT_FFPROBE_TIMEOUT_SECS)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FfprobeConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(default = "default_ffprobe_timeout_secs", skip_serializing_if = "is_default_ffprobe_timeout")]
    pub timeout: Option<u64>,
    #[serde(
        default = "default_metadata_ffprobe_analyze_duration",
        skip_serializing_if = "is_default_metadata_ffprobe_analyze_duration",
        deserialize_with = "deserialize_as_string"
    )]
    pub analyze_duration: String,
    #[serde(
        default = "default_metadata_ffprobe_probe_size",
        skip_serializing_if = "is_default_metadata_ffprobe_probe_size"
    )]
    pub probe_size: ByteSize,
    #[serde(
        default = "default_metadata_ffprobe_live_analyze_duration",
        skip_serializing_if = "is_default_metadata_ffprobe_live_analyze_duration",
        deserialize_with = "deserialize_as_string"
    )]
    pub live_analyze_duration: String,
    #[serde(
        default = "default_metadata_ffprobe_live_probe_size",
        skip_serializing_if = "is_default_metadata_ffprobe_live_probe_size"
    )]
    pub live_probe_size: ByteSize,
}

impl Default for FfprobeConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: default_ffprobe_timeout_secs(),
            analyze_duration: default_metadata_ffprobe_analyze_duration(),
            probe_size: default_metadata_ffprobe_probe_size(),
            live_analyze_duration: default_metadata_ffprobe_live_analyze_duration(),
            live_probe_size: default_metadata_ffprobe_live_probe_size(),
        }
    }
}

impl FfprobeConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && is_default_ffprobe_timeout(&self.timeout)
            && self.analyze_duration == default_metadata_ffprobe_analyze_duration()
            && self.probe_size == default_metadata_ffprobe_probe_size()
            && self.live_analyze_duration == default_metadata_ffprobe_live_analyze_duration()
            && self.live_probe_size == default_metadata_ffprobe_live_probe_size()
    }

    pub fn clean(&mut self) {
        if self.timeout.is_none_or(|timeout| timeout == 0) {
            self.timeout = default_ffprobe_timeout_secs();
        }
        if self.analyze_duration.trim().is_empty() {
            self.analyze_duration = default_metadata_ffprobe_analyze_duration();
        }
        if self.probe_size.as_str().trim().is_empty() {
            self.probe_size = default_metadata_ffprobe_probe_size();
        }
        if self.live_analyze_duration.trim().is_empty() {
            self.live_analyze_duration = default_metadata_ffprobe_live_analyze_duration();
        }
        if self.live_probe_size.as_str().trim().is_empty() {
            self.live_probe_size = default_metadata_ffprobe_live_probe_size();
        }
    }

    pub(super) fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.timeout = self.timeout.map(|timeout| timeout.max(MIN_DURATION_SECS));

        let analyze_duration_secs = MetadataUpdateConfigDto::parse_and_clamp_duration_with_required_unit(
            &self.analyze_duration,
            MIN_DURATION_SECS,
            "ffprobe.analyze_duration",
        )?;
        self.analyze_duration = MetadataUpdateConfigDto::canonicalize_seconds(analyze_duration_secs);

        let probe_size_bytes = self
            .probe_size
            .parse_bytes()
            .map_err(|err| {
                TuliproxError::ConfigMetadataUpdate(format!("Invalid size for `ffprobe.probe_size`: {err}"))
            })?
            .at_least_1();
        self.probe_size = ByteSize::new(MetadataUpdateConfigDto::canonicalize_size_bytes(probe_size_bytes.get()));

        let live_analyze_duration_secs = MetadataUpdateConfigDto::parse_and_clamp_duration_with_required_unit(
            &self.live_analyze_duration,
            MIN_DURATION_SECS,
            "ffprobe.live_analyze_duration",
        )?;
        self.live_analyze_duration = MetadataUpdateConfigDto::canonicalize_seconds(live_analyze_duration_secs);

        let live_probe_size_bytes = self
            .live_probe_size
            .parse_bytes()
            .map_err(|err| {
                TuliproxError::ConfigMetadataUpdate(format!("Invalid size for `ffprobe.live_probe_size`: {err}"))
            })?
            .at_least_1();
        self.live_probe_size =
            ByteSize::new(MetadataUpdateConfigDto::canonicalize_size_bytes(live_probe_size_bytes.get()));

        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TmdbConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tmdb_api_key", skip_serializing_if = "is_tmdb_default_api_key")]
    pub api_key: Option<String>,
    #[serde(default = "default_tmdb_rate_limit_ms", skip_serializing_if = "is_default_tmdb_rate_limit_ms")]
    pub rate_limit_ms: u64,
    #[serde(default = "default_tmdb_cache_duration_days", skip_serializing_if = "is_default_tmdb_cache_duration_days")]
    pub cache_duration_days: u32,
    #[serde(default = "default_tmdb_language", skip_serializing_if = "is_default_tmdb_language")]
    pub language: String,
    #[serde(default = "default_metadata_tmdb_cooldown", skip_serializing_if = "is_default_metadata_tmdb_cooldown")]
    pub cooldown: String,
    #[serde(default = "default_tmdb_match_threshold", skip_serializing_if = "is_default_tmdb_match_threshold")]
    pub match_threshold: u16,
}

impl Default for TmdbConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: default_tmdb_api_key(),
            rate_limit_ms: default_tmdb_rate_limit_ms(),
            cache_duration_days: default_tmdb_cache_duration_days(),
            language: default_tmdb_language(),
            cooldown: default_metadata_tmdb_cooldown(),
            match_threshold: default_tmdb_match_threshold(),
        }
    }
}

impl TmdbConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && self.api_key.as_ref().is_none_or(|api_key| api_key == TMDB_API_KEY)
            && self.rate_limit_ms == default_tmdb_rate_limit_ms()
            && self.cache_duration_days == default_tmdb_cache_duration_days()
            && self.language == default_tmdb_language()
            && self.cooldown == default_metadata_tmdb_cooldown()
            && self.match_threshold == default_tmdb_match_threshold()
    }

    pub(super) fn clean(&mut self) {
        self.api_key = self.api_key.take().and_then(|api_key| {
            let trimmed = api_key.trim();
            if trimmed.is_empty() || trimmed == TMDB_API_KEY {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
    }

    pub(super) fn prepare(&mut self) -> Result<(), TuliproxError> {
        let cooldown_secs =
            MetadataUpdateConfigDto::parse_and_clamp_duration(&self.cooldown, MIN_DURATION_SECS, "tmdb.cooldown")?;
        self.cooldown = MetadataUpdateConfigDto::canonicalize_seconds(cooldown_secs);
        self.match_threshold = self.match_threshold.clamp(0, 100);
        Ok(())
    }
}
