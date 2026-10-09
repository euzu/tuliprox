use super::{prepare_recording_config, RecordingConfigDto};
use crate::{
    defaults::{default_supported_video_extensions, is_default_supported_video_extensions},
    error::TuliproxError,
    utils::{is_blank_optional_str, is_blank_optional_string},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VideoConfigDto {
    #[serde(
        default = "default_supported_video_extensions",
        skip_serializing_if = "is_default_supported_video_extensions"
    )]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub web_search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingConfigDto>,
}

impl VideoConfigDto {
    pub fn is_empty(&self) -> bool {
        (self.extensions.is_empty() || is_default_supported_video_extensions(&self.extensions))
            && is_blank_optional_str(self.web_search.as_deref())
            && self.recording.as_ref().is_none_or(RecordingConfigDto::is_empty)
    }

    pub fn clean(&mut self) {
        if let Some(recording) = self.recording.as_mut() {
            recording.clean();
        }
        if self.recording.as_ref().is_some_and(RecordingConfigDto::is_empty) {
            self.recording = None;
        }
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if self.extensions.is_empty() {
            self.extensions = default_supported_video_extensions();
        }
        if let Some(recording) = self.recording.as_mut() {
            prepare_recording_config(recording)?;
        }
        Ok(())
    }
}
