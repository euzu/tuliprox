use crate::{
    model::{M3uPlaylistItem, PlaylistEntry, PlaylistItemType, StreamProperties, XtreamCluster, XtreamPlaylistItem},
    utils::{
        arc_str_option_serde, arc_str_serde, current_time_secs, extract_extension_from_url, is_blank_optional_string,
    },
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
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
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
    pub shared_joined_existing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_stream_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub technical: Option<StreamTechnicalInfo>,
    // EPG channel identifier for per-stream programme lookup.
    // None when no EPG is configured for the underlying input/target/item.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "arc_str_option_serde")]
    pub epg_channel_id: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_reference_ts: Option<i64>,
    #[serde(skip)]
    pub upstream_user_agent: Option<Arc<str>>,
}

impl StreamChannel {
    /// Returns a copy of this channel with the supplied EPG reference timestamp
    /// installed. Used by the M3U non-HLS path to thread an archive/catchup
    /// timestamp parsed from the request URL into the `StreamChannel` that the
    /// frontend then carries to the `stream_epg` endpoint.
    #[must_use]
    pub fn with_epg_reference_ts(mut self, epg_reference_ts: Option<i64>) -> Self {
        self.epg_reference_ts = epg_reference_ts;
        self
    }
}

pub fn create_stream_channel_with_type(
    target_id: u16,
    pli: &XtreamPlaylistItem,
    item_type: PlaylistItemType,
) -> StreamChannel {
    let mut stream_channel = pli.to_stream_channel(target_id);
    stream_channel.item_type = item_type;
    stream_channel.cluster = item_type.cluster();
    stream_channel
}

impl XtreamPlaylistItem {
    pub fn to_stream_channel(&self, target_id: u16) -> StreamChannel {
        let title = if self.title.is_empty() { Arc::clone(&self.name) } else { Arc::clone(&self.title) };
        StreamChannel {
            target_id,
            virtual_id: self.virtual_id,
            provider_id: self.provider_id,
            input_name: Arc::clone(&self.input_name),
            item_type: self.item_type,
            cluster: self.xtream_cluster,
            group: Arc::clone(&self.group),
            title,
            url: Arc::clone(&self.url),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: stream_technical_from_properties(self.additional_properties.as_ref(), self.url.as_ref()),
            epg_channel_id: self.epg_channel_id.clone(),
            epg_reference_ts: None,
            upstream_user_agent: self.upstream_user_agent.clone(),
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
            input_name: Arc::clone(&self.input_name),
            item_type: self.item_type,
            cluster: self.item_type.cluster(),
            group: Arc::clone(&self.group),
            title,
            url: Arc::clone(&self.url),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: stream_technical_from_properties(self.additional_properties.as_ref(), self.url.as_ref()),
            epg_channel_id: self.epg_channel_id.clone(),
            epg_reference_ts: None,
            upstream_user_agent: self.upstream_user_agent.clone(),
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
        .or_else(|| extract_extension_from_url(url).and_then(normalize_container))
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
    #[serde(default, with = "arc_str_serde")]
    pub provider: Arc<str>,
    pub addr: SocketAddr,
    pub client_ip: String,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub ts: u64,
    #[serde(default)]
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub country_code: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub session_token: Option<String>,
    #[serde(default)]
    pub preserved: bool,
    #[serde(default)]
    pub previous_session_id: Option<u64>,
}

pub struct StreamInfoParams<'a> {
    pub uid: u32,
    pub meter_uid: u32,
    pub username: &'a str,
    pub addr: &'a SocketAddr,
    pub client_ip: &'a str,
    pub provider: Arc<str>,
    pub stream_channel: StreamChannel,
    pub user_agent: String,
    pub country_code: Option<String>,
    pub session_token: Option<&'a str>,
}

impl StreamInfo {
    pub fn new(params: StreamInfoParams<'_>) -> Self {
        let now = current_time_secs();
        Self {
            uid: params.uid,
            meter_uid: params.meter_uid,
            username: params.username.to_string(),
            channel: params.stream_channel,
            provider: params.provider,
            addr: *params.addr,
            client_ip: params.client_ip.to_string(),
            user_agent: params.user_agent,
            ts: now,
            started_at: now,
            country_code: params.country_code,
            session_token: params.session_token.map(std::string::ToString::to_string),
            preserved: false,
            previous_session_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::create_stream_channel_with_type;
    use crate::{
        model::{M3uPlaylistItem, PlaylistItemType, XtreamCluster, XtreamPlaylistItem},
        utils::Internable,
    };

    #[test]
    fn create_stream_channel_with_type_keeps_cluster_consistent_with_item_type() {
        let playlist_item = XtreamPlaylistItem {
            virtual_id: 93_995,
            provider_id: 1,
            name: "Example".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Movies".intern(),
            title: "Example".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://provider.example/movie/93995.mkv".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: None,
            item_type: PlaylistItemType::Live,
            category_id: 0,
            input_name: "demo".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "1".intern(),
            upstream_user_agent: None,
        };

        let stream_channel = create_stream_channel_with_type(1, &playlist_item, PlaylistItemType::Video);

        assert_eq!(stream_channel.item_type, PlaylistItemType::Video);
        assert_eq!(stream_channel.cluster, XtreamCluster::Video);
    }

    #[test]
    fn to_stream_channel_preserves_input_name() {
        let playlist_item = XtreamPlaylistItem {
            virtual_id: 93_995,
            provider_id: 1,
            name: "Example".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Movies".intern(),
            title: "Example".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://provider.example/movie/93995.mkv".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: None,
            item_type: PlaylistItemType::Live,
            category_id: 0,
            input_name: "provider-input".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "1".intern(),
            upstream_user_agent: None,
        };

        let stream_channel = playlist_item.to_stream_channel(1);

        assert_eq!(stream_channel.input_name.as_ref(), "provider-input");
    }

    #[test]
    fn stream_channel_keeps_upstream_user_agent_internal() {
        let mut playlist_item = XtreamPlaylistItem {
            virtual_id: 1,
            provider_id: 1,
            name: "Example".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Live".intern(),
            title: "Example".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://provider.example/live/1.ts".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: None,
            item_type: PlaylistItemType::Live,
            category_id: 0,
            input_name: "provider-input".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "1".intern(),
            upstream_user_agent: Some("secret-provider-agent".intern()),
        };
        let stream_channel = playlist_item.to_stream_channel(1);
        playlist_item.upstream_user_agent = None;

        assert_eq!(stream_channel.upstream_user_agent.as_deref(), Some("secret-provider-agent"));
        assert!(!serde_json::to_string(&stream_channel).is_ok_and(|json| json.contains("secret-provider-agent")));
    }

    #[test]
    fn m3u_to_stream_channel_with_epg_reference_ts_propagates_value() {
        let pli = M3uPlaylistItem {
            virtual_id: 42,
            provider_id: "42".intern(),
            name: "Channel".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "G".intern(),
            title: "T".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "http://provider/live/42.m3u8".intern(),
            epg_channel_id: Some("ch1".intern()),
            input_name: "in".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: None,
            input_stream_id: "42".intern(),
            upstream_user_agent: None,
        };

        let channel = pli.to_stream_channel(7).with_epg_reference_ts(Some(1_700_000_000));

        assert_eq!(channel.target_id, 7);
        assert_eq!(channel.virtual_id, 42);
        assert_eq!(channel.epg_channel_id.as_deref(), Some("ch1"));
        assert_eq!(channel.epg_reference_ts, Some(1_700_000_000));
    }

    #[test]
    fn m3u_to_stream_channel_with_epg_reference_ts_explicit_none_matches_default() {
        let pli = M3uPlaylistItem {
            virtual_id: 42,
            provider_id: "42".intern(),
            name: "Channel".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "G".intern(),
            title: "T".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "http://provider/live/42.m3u8".intern(),
            epg_channel_id: None,
            input_name: "in".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: None,
            input_stream_id: "42".intern(),
            upstream_user_agent: None,
        };

        // to_stream_channel() defaults to None, and with_epg_reference_ts(None)
        // must keep that invariant.
        let default_channel = pli.to_stream_channel(7);
        let cleared_channel = pli.to_stream_channel(7).with_epg_reference_ts(None);

        assert_eq!(default_channel.epg_reference_ts, None);
        assert_eq!(cleared_channel.epg_reference_ts, None);
    }
}
