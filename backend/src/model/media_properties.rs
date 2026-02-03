use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

// Enum for Video Resolution
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VideoResolution {
    #[default]
    Unknown,
    SD,
    P720,
    P1080,
    P1440,
    P2160, // 4K
    P4320, // 8K
}

impl fmt::Display for VideoResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoResolution::SD => write!(f, "SD"),
            VideoResolution::P720 => write!(f, "720p HD"),
            VideoResolution::P1080 => write!(f, "1080p FHD"),
            VideoResolution::P1440 => write!(f, "1440p QHD"),
            VideoResolution::P2160 => write!(f, "2160p 4K"),
            VideoResolution::P4320 => write!(f, "4320p 8K"),
            VideoResolution::Unknown => write!(f, ""),
        }
    }
}

// Enum for Video Codec
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VideoCodec {
    #[default]
    Other,
    H264,
    H265,
    MPEG4,
    VC1,
    AV1,
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoCodec::H264 => write!(f, "H.264"), // or AVC
            VideoCodec::H265 => write!(f, "HEVC"),  // or x265
            VideoCodec::MPEG4 => write!(f, "MPEG4"),
            VideoCodec::VC1 => write!(f, "VC-1"),
            VideoCodec::AV1 => write!(f, "AV1"),
            VideoCodec::Other => write!(f, ""),
        }
    }
}

// Enum for Audio Codec
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioCodec {
    #[default]
    Other,
    AAC,
    AC3,
    EAC3,
    DTS,
    TrueHD,
    FLAC,
}

impl fmt::Display for AudioCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioCodec::AAC => write!(f, "AAC"),
            AudioCodec::AC3 => write!(f, "AC3"),
            AudioCodec::EAC3 => write!(f, "EAC3"),
            AudioCodec::DTS => write!(f, "DTS"),
            AudioCodec::TrueHD => write!(f, "TrueHD"),
            AudioCodec::FLAC => write!(f, "FLAC"),
            AudioCodec::Other => write!(f, ""),
        }
    }
}

// Enum for Audio Channels
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioChannels {
    #[default]
    Unknown,
    Mono,
    Stereo,
    Surround51,
    Surround71,
}

impl fmt::Display for AudioChannels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioChannels::Mono => write!(f, "1.0"),
            AudioChannels::Stereo => write!(f, "2.0"),
            AudioChannels::Surround51 => write!(f, "5.1"),
            AudioChannels::Surround71 => write!(f, "7.1"),
            AudioChannels::Unknown => write!(f, ""),
        }
    }
}

// Enum for Video Dynamic Range
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VideoDynamicRange {
    #[default]
    SDR,
    HDR,
    HDR10,
    HLG,
    DV, // Dolby Vision
}

impl fmt::Display for VideoDynamicRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoDynamicRange::SDR => write!(f, ""), // Don't explicitly tag SDR
            VideoDynamicRange::HDR => write!(f, "HDR"),
            VideoDynamicRange::HDR10 => write!(f, "HDR10"),
            VideoDynamicRange::HLG => write!(f, "HLG"),
            VideoDynamicRange::DV => write!(f, "DV"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VideoBitDepth {
    #[default]
    Eight,
    Ten,
}

impl fmt::Display for VideoBitDepth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoBitDepth::Eight => write!(f, ""),
            VideoBitDepth::Ten => write!(f, "10bit"),
        }
    }
}


/// A struct that holds all classified media quality features.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaQuality {
    pub resolution: VideoResolution,
    pub video_codec: VideoCodec,
    pub dynamic_range: VideoDynamicRange,
    pub bit_depth: VideoBitDepth,
    pub audio_codec: AudioCodec,
    pub audio_channels: AudioChannels,
}

impl MediaQuality {
    /// Formats the quality features into a string suitable for filenames, e.g., "1080p FHD H264 AAC 2.0".
    /// Returns an empty string if no relevant features are available to display.
    pub fn format_for_filename(&self, separator: &str) -> String {
        let mut parts = Vec::new();

        if self.resolution != VideoResolution::Unknown {
            parts.push(self.resolution.to_string());
        }
        if self.video_codec != VideoCodec::Other {
            parts.push(self.video_codec.to_string());
        }
        
        if self.bit_depth != VideoBitDepth::Eight {
             parts.push(self.bit_depth.to_string());
        }

        if self.dynamic_range != VideoDynamicRange::SDR {
            parts.push(self.dynamic_range.to_string());
        }
        
        if self.audio_codec != AudioCodec::Other {
            parts.push(self.audio_codec.to_string());
        }
        if self.audio_channels != AudioChannels::Unknown {
            parts.push(self.audio_channels.to_string());
        }
        
        // Filter empty strings from Display impls
        let valid_parts: Vec<&str> = parts.iter().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();

        valid_parts.join(separator)
    }

    fn from_ffprobe_info_audio(audio: Option<&str>) -> Option<(AudioCodec, AudioChannels)> {
        // Assuming the first audio stream is the primary one.
        let audio_info = audio.and_then(|v| serde_json::from_str::<Map<String, Value>>(v).ok())?;

        // Audio codec
        let audio_codec = get_value(&audio_info, &["codec_name"])
            .and_then(|v| v.as_str().map(str::to_lowercase))
            .map_or(AudioCodec::default(), |name| match name.as_str() {
                "aac" => AudioCodec::AAC,
                "ac3" => AudioCodec::AC3,
                "eac3" => AudioCodec::EAC3,
                "dts" => AudioCodec::DTS,
                "truehd" => AudioCodec::TrueHD,
                "flac" => AudioCodec::FLAC,
                _ => AudioCodec::default(),
            });

        // Audio channels
        let audio_channels = get_value(&audio_info, &["channels"])
            .and_then(|v| v.as_i64())
            .map_or(AudioChannels::default(), |ch| match ch {
                8 => AudioChannels::Surround71,
                6 => AudioChannels::Surround51,
                2 => AudioChannels::Stereo,
                1 => AudioChannels::Mono,
                _ => AudioChannels::default(),
            });

        Some((audio_codec, audio_channels))
    }

    fn from_ffprobe_info_video(video: Option<&str>) -> Option<(VideoResolution, VideoCodec, VideoDynamicRange, VideoBitDepth)> {
        let video_info = video.and_then(|v| serde_json::from_str::<Map<String, Value>>(v).ok())?;

        // 1. Classify video resolution from height
        let resolution = get_value(&video_info, &["height", "coded_height"])
            .and_then(|v| v.as_u64())
            .map_or(VideoResolution::default(), |h| match h {
                _ if h >= 4300 => VideoResolution::P4320,
                _ if h >= 2100 => VideoResolution::P2160,
                _ if h >= 1400 => VideoResolution::P1440,
                _ if h >= 1000 => VideoResolution::P1080,
                _ if h >= 700 => VideoResolution::P720,
                _ => VideoResolution::SD,
            });

        // 2. Classify video codec
        let video_codec = get_value(&video_info, &["codec_name"])
            .and_then(|v| v.as_str().map(str::to_lowercase))
            .map_or(VideoCodec::default(), |name| match name.as_str() {
                "h264" => VideoCodec::H264,
                "hevc" => VideoCodec::H265,
                "mpeg4" => VideoCodec::MPEG4,
                "vc1" => VideoCodec::VC1,
                "av1" => VideoCodec::AV1,
                _ => VideoCodec::default(),
            });

        // 3. Classify dynamic range
        let dynamic_range = {
            let tag_string = get_value(&video_info, &["codec_tag_string"])
                .and_then(|v| v.as_str().map(str::to_lowercase));

            if tag_string == Some("dovi".to_string()) {
                VideoDynamicRange::DV
            } else {
                get_value(&video_info, &["color_transfer"])
                    .and_then(|v| v.as_str().map(str::to_lowercase))
                    .map_or(VideoDynamicRange::SDR, |ct| match ct.as_str() {
                        "smpte2084" => VideoDynamicRange::HDR, // Generic HDR/HDR10
                        "arib-std-b67" => VideoDynamicRange::HLG,
                        _ => VideoDynamicRange::SDR,
                    })
            }
        };

        // 4. Classify bit depth
        let bit_depth = get_value(&video_info, &["pix_fmt"])
            .and_then(|v| v.as_str().map(ToString::to_string))
            .map_or(VideoBitDepth::Eight, |fmt| {
               if fmt.contains("10le") || fmt.contains("10be") {
                   VideoBitDepth::Ten
               } else {
                   VideoBitDepth::Eight
               }
            });

        Some((resolution, video_codec, dynamic_range, bit_depth))
    }

    /// Extracts media quality information from an `ffprobe` info block.
    /// The `info_block` is expected to be a `serde_json::Value` object encoded as string.
    pub fn from_ffprobe_info(audio: Option<&str>, video: Option<&str>) -> Option<Self> {
        // We attempt to parse whatever is available. If both missing, None.
        if audio.is_none() && video.is_none() {
            return None;
        }
        
        let (resolution, video_codec, dynamic_range, bit_depth) = Self::from_ffprobe_info_video(video)
            .unwrap_or((VideoResolution::default(), VideoCodec::default(), VideoDynamicRange::default(), VideoBitDepth::default()));
        
        let (audio_codec, audio_channels) = Self::from_ffprobe_info_audio(audio)
             .unwrap_or((AudioCodec::default(), AudioChannels::default()));

        Some(Self {
            resolution,
            video_codec,
            dynamic_range,
            bit_depth,
            audio_codec,
            audio_channels,
        })
    }

    /// Validates if the provided JSON string contains meaningful media information.
    /// Returns true if the string is valid JSON object and contains at least codec or dimension information.
    /// Returns false for empty arrays "[]" or objects without specific keys.
    pub fn is_valid_media_info(info: Option<&str>) -> bool {
        if let Some(json_str) = info {
             if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {
                 // API often returns [] for empty info
                 if let Some(arr) = json.as_array() {
                     return !arr.is_empty();
                 }
                 if let Some(obj) = json.as_object() {
                     // Check for minimal necessary fields
                     // For video: codec_name, width, height
                     // For audio: codec_name, channels
                     // We check generically if it looks populated
                     return obj.contains_key("codec_name") || obj.contains_key("width") || obj.contains_key("channels");
                 }
             }
        }
        false
    }
}


// Helper to get a value by trying a prioritized list of field names.
fn get_value(obj: &Map<String, Value>, fields: &[&str]) -> Option<Value> {
    for field in fields {
        if let Some(value) = obj.get(*field) {
            if !value.is_null() {
                return Some(value.clone());
            }
        }
    }
    None
}