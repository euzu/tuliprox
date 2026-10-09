use crate::{defaults::is_false, model::ClusterFlags};

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigTargetShareLiveStreams {
    #[serde(default, skip_serializing_if = "is_false")]
    pub hls: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub mpeg_ts: bool,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ConfigTargetShareLiveStreamsCompat {
    Legacy(bool),
    Structured(ConfigTargetShareLiveStreams),
}

fn deserialize_share_live_streams<'de, D>(deserializer: D) -> Result<ConfigTargetShareLiveStreams, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match <ConfigTargetShareLiveStreamsCompat as serde::Deserialize>::deserialize(deserializer)? {
        ConfigTargetShareLiveStreamsCompat::Legacy(enabled) => {
            ConfigTargetShareLiveStreams { hls: false, mpeg_ts: enabled }
        }
        ConfigTargetShareLiveStreamsCompat::Structured(config) => config,
    })
}

impl ConfigTargetShareLiveStreams {
    pub fn is_empty(&self) -> bool { !self.hls && !self.mpeg_ts }
}

/// Controls optional canonicalization of EPG data emitted for a target.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EpgOutputOptions {
    #[serde(default, skip_serializing_if = "is_false")]
    pub lowercase_ids: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub lowercase_xmltv_display_names: bool,
}

impl EpgOutputOptions {
    pub const fn is_empty(&self) -> bool { !self.lowercase_ids && !self.lowercase_xmltv_display_names }
}

#[derive(Debug, Copy, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeduplicateMatchBy {
    #[default]
    Caption,
    Name,
    Title,
}

#[derive(Debug, Copy, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeduplicateKeep {
    #[default]
    BestQuality,
    First,
}

/// Quality-aware duplicate removal: channels with the same normalized match
/// value (quality tokens stripped) collapse to a single entry.
#[derive(Debug, Copy, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeduplicateConfig {
    #[serde(default)]
    pub match_by: DeduplicateMatchBy,
    #[serde(default)]
    pub keep: DeduplicateKeep,
    /// Normalize accented characters in match keys ("Café HD" matches "Cafe FHD").
    #[serde(default)]
    pub match_as_ascii: bool,
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigTargetOptions {
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignore_logo: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    #[serde(alias = "required_epg")]
    pub clear_invalid_epg_ids: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_share_live_streams",
        skip_serializing_if = "ConfigTargetShareLiveStreams::is_empty"
    )]
    pub share_live_streams: ConfigTargetShareLiveStreams,
    #[serde(default, skip_serializing_if = "is_false")]
    pub remove_duplicates: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deduplicate: Option<DeduplicateConfig>,
    #[serde(default, skip_serializing_if = "EpgOutputOptions::is_empty")]
    pub epg_output: EpgOutputOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_redirect: Option<ClusterFlags>,
}

impl ConfigTargetOptions {
    pub fn is_empty(&self) -> bool {
        !self.ignore_logo
            && !self.clear_invalid_epg_ids
            && self.share_live_streams.is_empty()
            && !self.remove_duplicates
            && self.deduplicate.is_none()
            && self.epg_output.is_empty()
            && self.force_redirect.is_none_or(|f| f.has_full_flags() || f.is_empty())
    }

    pub const fn lowercase_epg_ids(&self) -> bool { self.epg_output.lowercase_ids }

    pub const fn lowercase_xmltv_display_names(&self) -> bool { self.epg_output.lowercase_xmltv_display_names }

    pub const fn clear_invalid_epg_ids(&self) -> bool { self.clear_invalid_epg_ids }

    pub fn share_live_hls_enabled(&self) -> bool { self.share_live_streams.hls }

    pub fn share_live_mpeg_ts_enabled(&self) -> bool { self.share_live_streams.mpeg_ts }

    pub fn share_live_any_enabled(&self) -> bool { self.share_live_hls_enabled() || self.share_live_mpeg_ts_enabled() }
}
