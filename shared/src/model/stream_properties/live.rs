use super::CatchupProperties;
use crate::utils::{
    arc_str_none_default_on_null, arc_str_option_null_if_empty_serde, deserialize_as_option_arc_str,
    deserialize_json_as_opt_string, deserialize_number_from_string, deserialize_number_from_string_or_zero,
    serialize_json_as_opt_string,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LiveStreamProperties {
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub name: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub category_id: u32,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub stream_id: u32,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub stream_icon: Arc<str>,
    #[serde(default, deserialize_with = "arc_str_none_default_on_null")]
    pub direct_source: Arc<str>,
    #[serde(default, with = "arc_str_option_null_if_empty_serde")]
    pub custom_sid: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub added: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub stream_type: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_as_option_arc_str")]
    pub epg_channel_id: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tv_archive: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub tv_archive_duration: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub is_adult: i32,
    // New fields for probing
    #[serde(
        default,
        serialize_with = "serialize_json_as_opt_string",
        deserialize_with = "deserialize_json_as_opt_string"
    )]
    pub video: Option<Arc<str>>,
    #[serde(
        default,
        serialize_with = "serialize_json_as_opt_string",
        deserialize_with = "deserialize_json_as_opt_string"
    )]
    pub audio: Option<Arc<str>>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub last_probed_timestamp: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub last_success_timestamp: Option<i64>,
    #[serde(default)]
    pub catchup: Option<CatchupProperties>,
    #[serde(default, deserialize_with = "deserialize_number_from_string_or_zero")]
    pub bitrate: u32,
}

impl LiveStreamProperties {
    /// Merges metadata learned from probing or playback without replacing newer provider data.
    pub fn merge_learned_metadata_from(&mut self, previous: &Self) -> bool {
        let mut changed = false;

        if self.video.is_none() && previous.video.is_some() {
            self.video.clone_from(&previous.video);
            changed = true;
        }

        if self.audio.is_none() && previous.audio.is_some() {
            self.audio.clone_from(&previous.audio);
            changed = true;
        }

        if previous.bitrate > self.bitrate {
            self.bitrate = previous.bitrate;
            changed = true;
        }

        if self.last_probed_timestamp.is_none() && previous.last_probed_timestamp.is_some() {
            self.last_probed_timestamp = previous.last_probed_timestamp;
            changed = true;
        }

        if self.last_success_timestamp.is_none() && previous.last_success_timestamp.is_some() {
            self.last_success_timestamp = previous.last_success_timestamp;
            changed = true;
        }

        changed
    }
}
