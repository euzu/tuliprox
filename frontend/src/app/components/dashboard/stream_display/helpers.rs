use crate::{
    app::ConfigContext,
    model::{BACKGROUND_TRANSFER_CLIENT_IP, BACKGROUND_TRANSFER_PROVIDER},
    utils::{format_duration, is_shared_hls_stream},
};
use gloo_utils::window;
use shared::{
    defaults::default_hls_session_ttl_secs,
    model::{PlaylistItemType, StreamChannel, StreamInfo, StreamInfoConfigDto, StreamTechnicalInfo},
    utils::current_time_secs,
};
use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{console, Element};
use yew::UseStateHandle;

const LIVE: &str = "Live";
const MOVIE: &str = "Movie";
const SERIES: &str = "Series";
const CATCHUP: &str = "Archive";
const HLS: &str = "HLS";
const DASH: &str = "DASH";

pub const ADAPTIVE_STREAM_CLEANUP_BUFFER_SECS: u64 = 5;
pub const ADAPTIVE_STREAM_CLEANUP_INTERVAL_MILLIS: u32 = 5_000;

pub fn is_stream_metrics_enabled(config_ctx: &ConfigContext) -> bool {
    config_ctx
        .config
        .as_ref()
        .and_then(|cfg| cfg.config.reverse_proxy.as_ref())
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .is_some_and(|stream| stream.metrics_enabled)
}

pub fn get_stream_info_config(config_ctx: &ConfigContext) -> Option<Rc<StreamInfoConfigDto>> {
    config_ctx
        .config
        .as_ref()
        .and_then(|cfg| cfg.config.web_ui.as_ref())
        .and_then(|web_ui| web_ui.stream_info.clone())
        .map(Rc::new)
}

pub fn get_adaptive_session_ttl_secs(config_ctx: &ConfigContext) -> u64 {
    config_ctx
        .config
        .as_ref()
        .and_then(|cfg| cfg.config.reverse_proxy.as_ref())
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map_or_else(default_hls_session_ttl_secs, |stream| stream.hls_session_ttl_secs)
}

fn is_catchup_session_token(session_token: &str) -> bool {
    session_token.starts_with("m3u-catchup|")
        || session_token.starts_with("catchup|")
        || session_token.contains("|archive|")
        || session_token.contains("|timeshift_abs|")
}

pub fn is_adaptive_session_stream(stream: &StreamInfo) -> bool {
    // Soft-preserve keeps archive/HLS rows between short segment sockets; this TTL path is what
    // eventually hides them after the session ends (without a full page reload).
    if stream.channel.item_type == PlaylistItemType::Catchup {
        return true;
    }
    if stream.session_token.as_deref().is_some_and(is_catchup_session_token) {
        return true;
    }
    let url = stream.channel.url.as_ref().to_ascii_lowercase();
    if url.contains(".m3u8") || url.contains(".mpd") {
        return true;
    }
    stream.session_token.is_some() && (stream.channel.item_type.is_live_adaptive() || is_shared_hls_stream(stream))
}

pub fn is_background_transfer_stream(stream: &StreamInfo) -> bool {
    stream.client_ip == BACKGROUND_TRANSFER_CLIENT_IP && stream.provider.as_ref() == BACKGROUND_TRANSFER_PROVIDER
}

pub fn filter_visible_streams(
    streams: Option<Vec<std::rc::Rc<StreamInfo>>>,
    adaptive_last_seen: &HashMap<u32, u64>,
    now_secs: u64,
    adaptive_ttl_secs: u64,
) -> Option<Vec<std::rc::Rc<StreamInfo>>> {
    streams.map(|streams| {
        streams
            .into_iter()
            .filter(|stream| {
                if !is_adaptive_session_stream(stream) {
                    return true;
                }

                adaptive_last_seen.get(&stream.uid).is_none_or(|last_seen| {
                    now_secs.saturating_sub(*last_seen)
                        <= adaptive_ttl_secs.saturating_add(ADAPTIVE_STREAM_CLEANUP_BUFFER_SECS)
                })
            })
            .collect()
    })
}

pub fn refresh_adaptive_last_seen(
    adaptive_last_seen: &UseStateHandle<HashMap<u32, u64>>,
    streams: &Option<Vec<std::rc::Rc<StreamInfo>>>,
) {
    let now = current_time_secs();
    adaptive_last_seen.set(compute_adaptive_last_seen((**adaptive_last_seen).clone(), streams, now));
}

fn compute_adaptive_last_seen(
    mut next: HashMap<u32, u64>,
    streams: &Option<Vec<std::rc::Rc<StreamInfo>>>,
    now: u64,
) -> HashMap<u32, u64> {
    if let Some(streams) = streams {
        let adaptive_uids: HashSet<u32> =
            streams.iter().filter(|stream| is_adaptive_session_stream(stream)).map(|stream| stream.uid).collect();

        next.retain(|uid, _| adaptive_uids.contains(uid));

        for stream in streams {
            if is_adaptive_session_stream(stream) {
                // Freeze last_seen for all preserved rows (including shared) so TTL can hide ended sessions.
                if !stream.preserved || !next.contains_key(&stream.uid) {
                    next.insert(stream.uid, now);
                }
            } else {
                next.remove(&stream.uid);
            }
        }
    } else {
        next.clear();
    }

    next
}

fn adaptive_tech_label(item_type: PlaylistItemType) -> Option<&'static str> {
    match item_type {
        PlaylistItemType::LiveHls => Some(HLS),
        PlaylistItemType::LiveDash => Some(DASH),
        _ => None,
    }
}

pub fn build_technical_chips(
    item_type: PlaylistItemType,
    technical: Option<&StreamTechnicalInfo>,
) -> Vec<(String, &'static str)> {
    let mut chips = Vec::new();
    let Some(tech) = technical else {
        if let Some(label) = adaptive_tech_label(item_type) {
            chips.push((label.to_string(), "tp__stream-display__chip--container"));
        }
        return chips;
    };

    let type_label = adaptive_tech_label(item_type);
    if let Some(label) = type_label {
        chips.push((label.to_string(), "tp__stream-display__chip--container"));
    }
    if !tech.container.is_empty() {
        let container = tech.container.to_ascii_uppercase();
        // LiveHls/Dash already add HLS/DASH from item_type; metrics often repeat the same container.
        let duplicate_type_label = type_label.is_some_and(|label| label.eq_ignore_ascii_case(&container));
        if !duplicate_type_label {
            chips.push((container, "tp__stream-display__chip--container"));
        }
    }
    if !tech.video_codec.is_empty() {
        chips.push((tech.video_codec.clone(), "tp__stream-display__chip--video-codec"));
    }
    if !tech.resolution.is_empty() {
        chips.push((tech.resolution.clone(), "tp__stream-display__chip--resolution"));
    }
    if !tech.fps.is_empty() {
        chips.push((format!("{} FPS", tech.fps), "tp__stream-display__chip--fps"));
    }
    if !tech.audio_codec.is_empty() {
        chips.push((tech.audio_codec.clone(), "tp__stream-display__chip--audio-codec"));
    }
    if !tech.audio_channels.is_empty() {
        chips.push((tech.audio_channels.clone(), "tp__stream-display__chip--audio-channels"));
    }

    chips
}

pub fn render_cluster(channel: &StreamChannel) -> &'static str {
    match channel.item_type {
        PlaylistItemType::LiveUnknown | PlaylistItemType::Live => LIVE,
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => MOVIE,
        PlaylistItemType::Series
        | PlaylistItemType::SeriesInfo
        | PlaylistItemType::LocalSeries
        | PlaylistItemType::LocalSeriesInfo => SERIES,
        PlaylistItemType::Catchup => CATCHUP,
        PlaylistItemType::LiveHls => HLS,
        PlaylistItemType::LiveDash => DASH,
    }
}

pub fn update_timestamps() {
    let window = window();
    let Some(document) = window.document() else {
        return;
    };
    let spans = match document.query_selector_all("span[data-ts]") {
        Ok(spans) => spans,
        Err(err) => {
            console::error_1(&err);
            return;
        }
    };
    for i in 0..spans.length() {
        if let Some(node) = spans.item(i) {
            let Ok(el) = node.dyn_into::<Element>() else {
                console::error_1(&JsValue::from_str("failed to convert timestamp node to Element"));
                continue;
            };
            if let Some(ts_str) = el.get_attribute("data-ts") {
                if let Ok(ts) = ts_str.parse::<u64>() {
                    el.set_inner_html(&format_duration(current_time_secs().saturating_sub(ts)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compute_adaptive_last_seen;
    use shared::{
        model::{PlaylistItemType, StreamChannel, StreamInfo, XtreamCluster},
        utils::Internable,
    };
    use std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        rc::Rc,
    };

    fn test_stream(uid: u32, item_type: PlaylistItemType, preserved: bool, has_session: bool) -> Rc<StreamInfo> {
        Rc::new(StreamInfo {
            uid,
            meter_uid: 0,
            username: "user".to_string(),
            channel: StreamChannel {
                target_id: 1,
                virtual_id: uid,
                provider_id: 1,
                item_type,
                cluster: XtreamCluster::try_from(item_type).unwrap_or(XtreamCluster::Live),
                group: "Group".intern(),
                title: "Title".intern(),
                url: "http://example.com/stream.m3u8".intern(),
                input_name: "input".intern(),
                shared: false,
                shared_joined_existing: None,
                shared_stream_id: None,
                technical: None,
                epg_channel_id: None,
                epg_reference_ts: None,
                upstream_user_agent: None,
            },
            provider: "provider".intern(),
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            client_ip: "127.0.0.1:1234".to_string(),
            user_agent: String::new(),
            ts: 0,
            started_at: 0,
            country_code: None,
            session_token: has_session.then(|| "session".to_string()),
            preserved,
            previous_session_id: None,
        })
    }

    fn test_shared_hls_stream(uid: u32, preserved: bool, has_session: bool) -> Rc<StreamInfo> {
        let mut stream = (*test_stream(uid, PlaylistItemType::LiveHls, preserved, has_session)).clone();
        stream.channel.shared = true;
        Rc::new(stream)
    }

    #[test]
    fn compute_adaptive_last_seen_prunes_missing_uids() {
        let existing = HashMap::from([(1, 100), (2, 200), (3, 300)]);
        let streams = Some(vec![
            test_stream(2, PlaylistItemType::LiveHls, true, true),
            test_stream(4, PlaylistItemType::LiveDash, false, true),
            test_stream(9, PlaylistItemType::Live, false, false),
        ]);

        let refreshed = compute_adaptive_last_seen(existing, &streams, 999);

        assert_eq!(refreshed.get(&2), Some(&200));
        assert_eq!(refreshed.get(&4), Some(&999));
        assert!(!refreshed.contains_key(&1));
        assert!(!refreshed.contains_key(&3));
        assert!(!refreshed.contains_key(&9));
    }

    #[test]
    fn compute_adaptive_last_seen_freezes_preserved_shared_hls() {
        let existing = HashMap::from([(2, 200)]);
        let streams = Some(vec![test_shared_hls_stream(2, true, true)]);

        let refreshed = compute_adaptive_last_seen(existing, &streams, 999);

        assert_eq!(refreshed.get(&2), Some(&200));
    }

    #[test]
    fn compute_adaptive_last_seen_tracks_catchup_session_streams() {
        let existing = HashMap::from([(5, 100)]);
        let streams = Some(vec![
            test_stream(5, PlaylistItemType::Catchup, true, true),
            test_stream(6, PlaylistItemType::Catchup, false, true),
        ]);

        let refreshed = compute_adaptive_last_seen(existing, &streams, 999);

        // Preserved catchup keeps prior last_seen (sticky between segments).
        assert_eq!(refreshed.get(&5), Some(&100));
        assert_eq!(refreshed.get(&6), Some(&999));
    }

    #[test]
    fn filter_visible_streams_keeps_preserved_catchup_within_adaptive_ttl() {
        use super::filter_visible_streams;

        let streams = Some(vec![
            test_stream(1, PlaylistItemType::Catchup, true, true),
            test_stream(2, PlaylistItemType::Video, false, true),
        ]);
        let last_seen = HashMap::from([(1, 1_000u64)]);
        let ttl = 30u64;
        let visible = filter_visible_streams(streams, &last_seen, 1_000 + ttl, ttl).unwrap();
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().any(|stream| stream.channel.item_type == PlaylistItemType::Catchup));
    }

    #[test]
    fn filter_visible_streams_hides_preserved_catchup_past_adaptive_ttl() {
        use super::{filter_visible_streams, ADAPTIVE_STREAM_CLEANUP_BUFFER_SECS};

        let streams = Some(vec![
            test_stream(1, PlaylistItemType::Catchup, true, true),
            test_stream(2, PlaylistItemType::Video, false, true),
        ]);
        let last_seen = HashMap::from([(1, 1_000u64)]);
        let ttl = 30u64;
        let visible =
            filter_visible_streams(streams, &last_seen, 1_000 + ttl + ADAPTIVE_STREAM_CLEANUP_BUFFER_SECS + 1, ttl)
                .unwrap();
        // Soft-preserve is for segment gaps only; past TTL the row must leave Streams without reload.
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].channel.item_type, PlaylistItemType::Video);
    }

    #[test]
    fn filter_visible_streams_hides_stale_active_catchup_past_adaptive_ttl() {
        use super::{filter_visible_streams, ADAPTIVE_STREAM_CLEANUP_BUFFER_SECS};

        let streams = Some(vec![
            test_stream(1, PlaylistItemType::Catchup, false, true),
            test_stream(2, PlaylistItemType::Video, false, true),
        ]);
        let last_seen = HashMap::from([(1, 1_000u64)]);
        let ttl = 30u64;
        let visible =
            filter_visible_streams(streams, &last_seen, 1_000 + ttl + ADAPTIVE_STREAM_CLEANUP_BUFFER_SECS + 1, ttl)
                .unwrap();
        // Active rows refresh last_seen in the UI; a frozen last_seen past TTL must hide.
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].channel.item_type, PlaylistItemType::Video);
    }

    #[test]
    fn build_technical_chips_dedupes_hls_type_and_container() {
        use super::build_technical_chips;
        use shared::model::StreamTechnicalInfo;

        let tech =
            StreamTechnicalInfo { container: "hls".to_string(), video_codec: "h264".to_string(), ..Default::default() };
        let chips = build_technical_chips(PlaylistItemType::LiveHls, Some(&tech));
        let labels: Vec<_> = chips.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["HLS", "h264"]);
    }
}
