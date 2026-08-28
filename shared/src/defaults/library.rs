//! Library defaults: extensions, categories, thumbnails, processing-order
//! predicate, video-DTO empty predicate, config-target-options predicate.

use crate::model::{LibraryMetadataFormat, ProcessingOrder, VideoConfigDto};

pub const DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS: &[&str] = &["mp4", "mkv", "avi", "mov", "ts", "m4v", "webm"];

pub fn default_supported_library_extensions() -> Vec<String> {
    DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect()
}

pub fn is_default_supported_library_extensions(v: &[String]) -> bool {
    v.len() == DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS.len()
        && v.iter().zip(DEFAULT_SUPPORTED_LIBRARY_EXTENSIONS).all(|(a, b)| a == b)
}

pub const DEFAULT_VIDEO_EXTENSIONS: &[&str] = &["mkv", "avi", "mp4", "mpeg", "divx", "mov"];

pub fn default_supported_video_extensions() -> Vec<String> {
    DEFAULT_VIDEO_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect()
}

pub fn is_default_supported_video_extensions(v: &[String]) -> bool {
    v.len() == DEFAULT_VIDEO_EXTENSIONS.len() && v.iter().zip(DEFAULT_VIDEO_EXTENSIONS).all(|(a, b)| a == b)
}

pub fn default_storage_formats() -> Vec<LibraryMetadataFormat> {
    vec![]
}
pub fn default_movie_category() -> String {
    String::from("Local Movies")
}
pub fn default_series_category() -> String {
    String::from("Local TV Shows")
}

pub fn default_thumbnail_width() -> u32 {
    320
}
pub fn default_thumbnail_height() -> u32 {
    180
}
pub fn default_thumbnail_quality() -> u8 {
    75
}

pub fn is_default_processing_order(p: &ProcessingOrder) -> bool {
    *p == ProcessingOrder::default()
}

pub const fn default_probe_live_interval() -> u32 {
    120
}
pub const fn is_default_probe_live_interval(v: &u32) -> bool {
    *v == default_probe_live_interval()
}

pub fn is_none_or_empty_video(video: &Option<VideoConfigDto>) -> bool {
    video.as_ref().is_none_or(VideoConfigDto::is_empty)
}

// Clippy's method-path suggestion here names a private module and does not
// compile; the closure is kept deliberately.
#[allow(clippy::redundant_closure_for_method_calls)]
pub fn is_config_target_options_empty(v: &Option<crate::model::ConfigTargetOptions>) -> bool {
    v.as_ref().is_none_or(|c| c.is_empty())
}
