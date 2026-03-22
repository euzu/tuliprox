use crate::{
    model::{M3uPlaylistItem, PlaylistEntry, PlaylistItemType, StreamProperties, XtreamCluster, XtreamPlaylistItem},
    utils::{arc_str_serde, current_time_secs, extract_extension_from_url, is_blank_optional_string},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StreamTechnicalInfo {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub container: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolution: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fps: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_codec: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub audio_codec: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub audio_channels: String,
}

impl StreamTechnicalInfo {
    pub fn is_empty(&self) -> bool {
        self.container.is_empty()
            && self.resolution.is_empty()
            && self.fps.is_empty()
            && self.video_codec.is_empty()
            && self.audio_codec.is_empty()
            && self.audio_channels.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamChannel {
    pub target_id: u16,
    pub virtual_id: u32,
    pub provider_id: u32,
    pub item_type: PlaylistItemType,
    pub cluster: XtreamCluster,
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub url: Arc<str>,
    pub shared: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub technical: Option<StreamTechnicalInfo>,
}

pub fn create_stream_channel_with_type(
    target_id: u16,
    pli: &XtreamPlaylistItem,
    item_type: PlaylistItemType,
) -> StreamChannel {
    let mut stream_channel = pli.to_stream_channel(target_id);
    stream_channel.item_type = item_type;
    stream_channel
}

impl XtreamPlaylistItem {
    pub fn to_stream_channel(&self, target_id: u16) -> StreamChannel {
        let title = if self.title.is_empty() { Arc::clone(&self.name) } else { Arc::clone(&self.title) };
        StreamChannel {
            target_id,
            virtual_id: self.virtual_id,
            provider_id: self.provider_id,
            item_type: self.item_type,
            cluster: self.xtream_cluster,
            group: Arc::clone(&self.group),
            title,
            url: Arc::clone(&self.url),
            shared: false,
            technical: stream_technical_from_properties(self.additional_properties.as_ref(), self.url.as_ref()),
        }
    }
}

impl M3uPlaylistItem {
    pub fn to_stream_channel(&self, target_id: u16) -> StreamChannel {
        let title = if self.title.is_empty() { Arc::clone(&self.name) } else { Arc::clone(&self.title) };
        StreamChannel {
            target_id,
            virtual_id: self.virtual_id,
            provider_id: self.get_provider_id().unwrap_or_default(),
            item_type: self.item_type,
            cluster: XtreamCluster::try_from(self.item_type).unwrap_or(XtreamCluster::Live),
            group: Arc::clone(&self.group),
            title,
            url: Arc::clone(&self.url),
            shared: false,
            technical: stream_technical_from_properties(self.additional_properties.as_ref(), self.url.as_ref()),
        }
    }
}

fn stream_technical_from_properties(properties: Option<&StreamProperties>, url: &str) -> Option<StreamTechnicalInfo> {
    let (video_raw, audio_raw, container_raw) = match properties {
        Some(StreamProperties::Live(live)) => (live.video.as_deref(), live.audio.as_deref(), None),
        Some(StreamProperties::Video(video)) => (
            video.details.as_ref().and_then(|d| d.video.as_deref()),
            video.details.as_ref().and_then(|d| d.audio.as_deref()),
            Some(video.container_extension.as_ref()),
        ),
        Some(StreamProperties::Episode(episode)) => {
            (episode.video.as_deref(), episode.audio.as_deref(), Some(episode.container_extension.as_ref()))
        }
        _ => (None, None, None),
    };

    let video_json = video_raw.and_then(parse_probe_json);
    let audio_json = audio_raw.and_then(parse_probe_json);

    let mut info = StreamTechnicalInfo::default();

    if let Some(video) = video_json.as_ref() {
        info.resolution = parse_resolution(video).unwrap_or_default();
        info.fps = parse_fps(video).unwrap_or_default();
        info.video_codec = parse_video_codec(video).unwrap_or_default();
    }
    if let Some(audio) = audio_json.as_ref() {
        info.audio_codec = parse_audio_codec(audio).unwrap_or_default();
        info.audio_channels = parse_audio_channels(audio).unwrap_or_default();
    }

    info.container = container_raw
        .and_then(normalize_container)
        .or_else(|| extract_extension_from_url(url).and_then(|ext| normalize_container(&ext)))
        .unwrap_or_default();

    if info.is_empty() {
        None
    } else {
        Some(info)
    }
}

fn parse_probe_json(raw: &str) -> Option<Value> {
    if raw.trim().is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(raw).ok()
    }
}

fn get_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn get_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|entry| entry.as_u64().or_else(|| entry.as_str().and_then(|s| s.parse::<u64>().ok())))
    })
}

fn normalize_container(raw: &str) -> Option<String> {
    let value = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    let normalized = match value.as_str() {
        "ts" => "mpegts",
        "m3u8" => "hls",
        "mpd" => "dash",
        "mp4" | "mkv" | "avi" | "flv" | "mov" | "wmv" | "webm" | "mpegts" | "mpeg" | "mpg" | "ogg" | "ogv" | "3gp"
        | "hls" | "dash" | "m4v" | "asf" | "vob" | "mts" | "m2ts" => &value,
        _ => return None,
    };
    Some(normalized.to_string())
}

fn normalize_video_codec(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "h264" => "H.264".to_string(),
        "hevc" | "h265" => "HEVC".to_string(),
        "mpeg4" => "MPEG4".to_string(),
        "av1" => "AV1".to_string(),
        "vp9" => "VP9".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

fn normalize_audio_codec(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "aac" => "AAC".to_string(),
        "ac3" => "AC3".to_string(),
        "eac3" => "EAC3".to_string(),
        "dts" => "DTS".to_string(),
        "truehd" => "TRUEHD".to_string(),
        "flac" => "FLAC".to_string(),
        "mp3" => "MP3".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

fn parse_resolution(video: &Value) -> Option<String> {
    let width = get_u64(video, &["width", "coded_width"]);
    let height = get_u64(video, &["height", "coded_height"]);
    match (width, height) {
        (Some(w), Some(h)) => Some(format!("{w}x{h}")),
        (None, Some(h)) => Some(format!("{h}p")),
        _ => None,
    }
}

fn parse_rate_value(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((num, den)) = trimmed.split_once('/') {
        let numerator = num.parse::<f64>().ok()?;
        let denominator = den.parse::<f64>().ok()?;
        if denominator.abs() <= f64::EPSILON {
            None
        } else {
            Some(numerator / denominator)
        }
    } else {
        trimmed.parse::<f64>().ok()
    }
}

fn parse_fps(video: &Value) -> Option<String> {
    let raw_rate = get_str(video, &["avg_frame_rate", "r_frame_rate"])?;
    let fps = parse_rate_value(raw_rate)?;
    if fps <= 0.0 {
        return None;
    }
    let rounded = fps.round();
    if (fps - rounded).abs() < 0.01 {
        Some(format!("{rounded:.0}"))
    } else {
        Some(format!("{fps:.2}"))
    }
}

fn parse_video_codec(video: &Value) -> Option<String> { get_str(video, &["codec_name"]).map(normalize_video_codec) }

fn parse_audio_codec(audio: &Value) -> Option<String> { get_str(audio, &["codec_name"]).map(normalize_audio_codec) }

fn parse_audio_channels(audio: &Value) -> Option<String> {
    let channels = get_u64(audio, &["channels"])?;
    let mapped = match channels {
        1 => "MONO".to_string(),
        2 => "STEREO".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        _ => channels.to_string(),
    };
    Some(mapped)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamInfo {
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub meter_uid: u32,
    pub username: String,
    pub channel: StreamChannel,
    pub provider: String,
    pub addr: SocketAddr,
    pub client_ip: String,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub ts: u64,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub session_token: Option<String>,
    #[serde(default)]
    pub preserved: bool,
}

impl StreamInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        uid: u32,
        meter_uid: u32,
        username: &str,
        addr: &SocketAddr,
        client_ip: &str,
        provider: &str,
        stream_channel: StreamChannel,
        user_agent: String,
        country: Option<String>,
        session_token: Option<&str>,
    ) -> Self {
        Self {
            uid,
            meter_uid,
            username: username.to_string(),
            channel: stream_channel,
            provider: provider.to_string(),
            addr: *addr,
            client_ip: client_ip.to_string(),
            user_agent,
            ts: current_time_secs(),
            country,
            session_token: session_token.map(|token| token.to_string()),
            preserved: false,
        }
    }
}
