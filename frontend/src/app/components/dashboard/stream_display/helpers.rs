use crate::app::ConfigContext;
use gloo_utils::window;
use shared::{
    model::{PlaylistItemType, StreamChannel, StreamInfo, StreamTechnicalInfo},
    utils::{current_time_secs, default_hls_session_ttl_secs},
};
use std::collections::HashMap;
use wasm_bindgen::JsCast;
use web_sys::Element;
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

pub fn get_adaptive_session_ttl_secs(config_ctx: &ConfigContext) -> u64 {
    config_ctx
        .config
        .as_ref()
        .and_then(|cfg| cfg.config.reverse_proxy.as_ref())
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map_or_else(default_hls_session_ttl_secs, |stream| stream.hls_session_ttl_secs)
}

pub fn is_adaptive_session_stream(stream: &StreamInfo) -> bool {
    stream.session_token.is_some() && stream.channel.item_type.is_live_adaptive()
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
    let mut next = (**adaptive_last_seen).clone();

    if let Some(streams) = streams {
        for stream in streams {
            if is_adaptive_session_stream(stream) {
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

    adaptive_last_seen.set(next);
}

pub fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

pub fn format_bandwidth(rate_kbps: u32) -> String {
    if rate_kbps == 0 {
        return "-".to_string();
    }
    if rate_kbps >= 1_048_576 {
        format!("{:.1} GB/s", f64::from(rate_kbps) / 1_048_576.0)
    } else if rate_kbps >= 1024 {
        format!("{:.1} MB/s", f64::from(rate_kbps) / 1024.0)
    } else {
        format!("{rate_kbps} KB/s")
    }
}

pub fn format_transferred(total_kb: u32) -> String {
    if total_kb == 0 {
        return "-".to_string();
    }
    if total_kb >= 1_048_576 {
        format!("{:.2} GB", f64::from(total_kb) / 1_048_576.0)
    } else if total_kb >= 1024 {
        format!("{:.1} MB", f64::from(total_kb) / 1024.0)
    } else {
        format!("{total_kb} KB")
    }
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

    if let Some(label) = adaptive_tech_label(item_type) {
        chips.push((label.to_string(), "tp__stream-display__chip--container"));
    }
    if !tech.container.is_empty() {
        chips.push((tech.container.to_ascii_uppercase(), "tp__stream-display__chip--container"));
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
    let document = window.document().unwrap();
    let spans = document.query_selector_all("span[data-ts]").unwrap();
    for i in 0..spans.length() {
        if let Some(node) = spans.item(i) {
            let el: Element = node.dyn_into().unwrap();
            if let Some(ts_str) = el.get_attribute("data-ts") {
                if let Ok(ts) = ts_str.parse::<u64>() {
                    el.set_inner_html(&format_duration(current_time_secs() - ts));
                }
            }
        }
    }
}
