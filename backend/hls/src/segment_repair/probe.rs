use super::{
    diagnostics::{decide_repair, SegmentProbeStreamType},
    window::{RepairStatus, RepairVideoCodec},
    HlsSegmentCache, RepairRemuxDroppedStream, RepairRemuxStreamSelection, SegmentProbe, SegmentProbeStream,
    StagedCacheObject, WarningCounters,
};
use log::warn;
use serde_json::Value;
use sha2::{Digest, Sha256};
use shared::model::HlsSegmentRepairMode;
use std::{
    fmt, io,
    path::{Path, PathBuf},
};
use tokio::{fs, fs::File, io::AsyncReadExt};
use tuliprox_core::model::HlsSegmentRepairConfig;

pub(super) fn parse_probe(json: &str, warnings: WarningCounters) -> Result<SegmentProbe, String> {
    let value = serde_json::from_str::<Value>(json).map_err(|_| "invalid_probe_json".to_string())?;
    let streams = value.get("streams").and_then(Value::as_array).ok_or_else(|| "missing_streams".to_string())?;
    let mut probe = SegmentProbe { stream_count: streams.len(), warnings, ..SegmentProbe::default() };
    if let Some(format) = value.get("format") {
        probe.duration_ms = format.get("duration").and_then(Value::as_str).and_then(parse_seconds_ms_u64);
        probe.size =
            format.get("size").and_then(Value::as_str).and_then(|value| value.parse().ok()).unwrap_or_default();
    }
    for stream in streams {
        let codec_type = stream.get("codec_type").and_then(Value::as_str);
        let codec_name = stream.get("codec_name").and_then(Value::as_str).map(ToOwned::to_owned);
        let start_time_ms = stream.get("start_time").and_then(Value::as_str).and_then(parse_seconds_ms_i64);
        let extradata_size = stream
            .get("extradata_size")
            .and_then(Value::as_u64)
            .or_else(|| stream.get("extradata_size").and_then(Value::as_str).and_then(|value| value.parse().ok()));
        let stream_index =
            stream.get("index").and_then(parse_u32_value).map_or(probe.streams.len(), |value| value as usize);
        let probe_stream = SegmentProbeStream {
            index: stream_index,
            stream_type: match codec_type {
                Some("video") => SegmentProbeStreamType::Video,
                Some("audio") => SegmentProbeStreamType::Audio,
                _ => SegmentProbeStreamType::Other,
            },
            codec_name: codec_name.clone(),
            width: stream.get("width").and_then(parse_u32_value),
            height: stream.get("height").and_then(parse_u32_value),
            sample_rate: stream.get("sample_rate").and_then(parse_u32_value),
            channels: stream.get("channels").and_then(parse_u32_value),
        };
        match codec_type {
            Some("video") if probe.primary_video_codec.is_none() => {
                probe.primary_video_codec = codec_name;
                probe.primary_video_start_time_ms = start_time_ms;
                probe.primary_video_extradata_size = extradata_size;
            }
            Some("audio") if probe.primary_audio_codec.is_none() => {
                probe.primary_audio_codec = codec_name;
                probe.primary_audio_start_time_ms = start_time_ms;
            }
            _ => {}
        }
        probe.streams.push(probe_stream);
    }
    Ok(probe)
}

fn parse_u32_value(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|value| u32::try_from(value).ok()).or_else(|| {
        value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "N/A")
            .and_then(|value| value.parse::<u32>().ok())
    })
}

pub(super) fn detect_video_codec(probe: &SegmentProbe) -> RepairVideoCodec {
    match probe.primary_video_codec.as_deref() {
        Some("h264") => RepairVideoCodec::H264,
        Some("hevc" | "h265") => RepairVideoCodec::Hevc,
        _ => RepairVideoCodec::Unsupported,
    }
}

pub(super) fn select_repair_remux_streams(probe: &SegmentProbe) -> Result<RepairRemuxStreamSelection, String> {
    let mut mapped_streams = Vec::new();
    let mut dropped_streams = Vec::new();
    let mut has_video = false;
    for stream in &probe.streams {
        match stream.stream_type {
            SegmentProbeStreamType::Video if valid_video_stream(stream) => {
                has_video = true;
                mapped_streams.push(stream.index);
            }
            SegmentProbeStreamType::Audio if valid_audio_stream(stream) => {
                mapped_streams.push(stream.index);
            }
            SegmentProbeStreamType::Video => dropped_streams
                .push(RepairRemuxDroppedStream { index: stream.index, reason: "invalid-video-parameters" }),
            SegmentProbeStreamType::Audio => dropped_streams
                .push(RepairRemuxDroppedStream { index: stream.index, reason: "invalid-audio-parameters" }),
            SegmentProbeStreamType::Other => dropped_streams
                .push(RepairRemuxDroppedStream { index: stream.index, reason: "unsupported-stream-type" }),
        }
    }
    if !has_video {
        return Err("no_valid_video_stream".to_string());
    }
    Ok(RepairRemuxStreamSelection { mapped_streams, dropped_streams })
}

fn valid_video_stream(stream: &SegmentProbeStream) -> bool {
    stream.codec_name.as_deref().is_some_and(|codec| !codec.is_empty())
        && stream.width.unwrap_or_default() > 0
        && stream.height.unwrap_or_default() > 0
}

fn valid_audio_stream(stream: &SegmentProbeStream) -> bool {
    stream.codec_name.as_deref().is_some_and(|codec| !codec.is_empty())
        && stream.sample_rate.unwrap_or_default() > 0
        && stream.channels.unwrap_or_default() > 0
}

pub(super) fn validate_repair(
    config: &HlsSegmentRepairConfig,
    codec: RepairVideoCodec,
    raw: &SegmentProbe,
    fixed: &SegmentProbe,
    executed_level: HlsSegmentRepairMode,
    stream_selection: &RepairRemuxStreamSelection,
) -> Result<(), String> {
    if codec == RepairVideoCodec::Unsupported {
        return Err("unsupported_codec".to_string());
    }
    let expected_stream_count = if stream_selection.dropped_streams.is_empty() {
        raw.stream_count
    } else {
        stream_selection.mapped_streams.len()
    };
    if expected_stream_count != fixed.stream_count {
        return Err("stream_count_changed".to_string());
    }
    if raw.primary_video_codec != fixed.primary_video_codec {
        return Err("video_codec_changed".to_string());
    }
    if raw.primary_audio_codec != fixed.primary_audio_codec {
        return Err("audio_codec_changed".to_string());
    }
    if delta_u64(raw.duration_ms, fixed.duration_ms) > 250 {
        return Err("duration_delta_too_large".to_string());
    }
    if delta_i64(raw.primary_video_start_time_ms, fixed.primary_video_start_time_ms) > 250 {
        return Err("video_start_time_delta_too_large".to_string());
    }
    if delta_i64(raw.primary_audio_start_time_ms, fixed.primary_audio_start_time_ms) > 250 {
        return Err("audio_start_time_delta_too_large".to_string());
    }
    let raw_decision = decide_repair(codec, &raw.warnings);
    let fixed_decision = decide_repair(codec, &fixed.warnings);
    if raw_decision.required_level != HlsSegmentRepairMode::Off
        && fixed_decision.required_level != HlsSegmentRepairMode::Off
    {
        return Err("repair_triggers_remaining".to_string());
    }
    if raw.size > 0 {
        let allowed =
            raw.size.saturating_add(raw.size.saturating_mul(size_increase_percent(config, executed_level)) / 100);
        if fixed.size > allowed {
            return Err("size_increase_too_large".to_string());
        }
    }
    Ok(())
}

fn size_increase_percent(config: &HlsSegmentRepairConfig, level: HlsSegmentRepairMode) -> u64 {
    match level {
        HlsSegmentRepairMode::Off => 0,
        HlsSegmentRepairMode::Low => u64::from(config.size_increase.low_percent),
        HlsSegmentRepairMode::Medium => u64::from(config.size_increase.medium_percent),
        HlsSegmentRepairMode::High => u64::from(config.size_increase.high_percent),
    }
}

fn parse_seconds_ms_u64(value: &str) -> Option<u64> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    let whole_ms = whole.parse::<u64>().ok()?.checked_mul(1_000)?;
    let frac_ms = frac.chars().take(3).collect::<String>();
    let frac_ms = format!("{frac_ms:0<3}").parse::<u64>().ok()?;
    Some(whole_ms.saturating_add(frac_ms))
}

fn parse_seconds_ms_i64(value: &str) -> Option<i64> {
    let negative = value.starts_with('-');
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let parsed = i64::try_from(parse_seconds_ms_u64(unsigned)?).ok()?;
    Some(if negative { -parsed } else { parsed })
}

fn delta_u64(lhs: Option<u64>, rhs: Option<u64>) -> u64 {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => lhs.abs_diff(rhs),
        (None, None) => 0,
        _ => u64::MAX,
    }
}

fn delta_i64(lhs: Option<i64>, rhs: Option<i64>) -> u64 {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => lhs.abs_diff(rhs),
        (None, None) => 0,
        _ => u64::MAX,
    }
}

pub(crate) async fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

pub(crate) fn ffmpeg_identity_version() -> String { "system".to_string() }

pub(super) fn repair_output_path(raw_path: &Path) -> PathBuf {
    let suffix = fastrand::u64(..);
    let file_name = raw_path.file_name().and_then(|file_name| file_name.to_str()).unwrap_or("segment");
    raw_path.with_file_name(format!("{file_name}.repair.tmp.{suffix:016x}"))
}

pub(super) async fn cleanup_repair_output(path: &Path) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => warn!("HLS repair fixed staging cleanup deferred: error_kind={:?}", error.kind()),
    }
}

pub(super) async fn adopt_repair_output(
    segment_cache: &HlsSegmentCache,
    path: PathBuf,
    size: u64,
) -> io::Result<StagedCacheObject> {
    match segment_cache.adopt_staged_file(path.clone(), size) {
        Ok(staged) => Ok(staged),
        Err(error) => {
            cleanup_repair_output(&path).await;
            Err(error)
        }
    }
}

impl fmt::Display for RepairStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clean => f.write_str("clean"),
            Self::Fixed => f.write_str("fixed"),
            Self::PolicyLimited => f.write_str("policy_limited"),
            Self::Unsupported => f.write_str("unsupported"),
            Self::Timeout => f.write_str("timeout"),
            Self::RemuxFailed => f.write_str("remux_failed"),
            Self::ValidationFailed => f.write_str("validation_failed"),
        }
    }
}
