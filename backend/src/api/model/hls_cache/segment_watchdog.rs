#![allow(clippy::large_futures)]

use super::{
    CachedSegmentMetadata, HlsCacheObjectKey, HlsSegmentCache, StagedCacheObject,
    segment_repair::{
        HlsPostProcessingDeadline, HlsSegmentRepairObjectContext, ffmpeg_identity_version, run_command_with_deadline,
        sha256_file,
    },
};
use crate::model::{HlsCorruptSegmentWatchdogConfig, HlsCorruptSegmentWatchdogMode};
use log::debug;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs,
    sync::{Mutex, RwLock, Semaphore},
};

const WATCHDOG_COMMAND_VERSION: u32 = 1;
const WATCHDOG_METADATA_MAX_ENTRIES: usize = 4_096;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum HlsCorruptSegmentWatchdogStatus {
    Clean,
    DetectedCorrupt { packet_corrupt_count: u32 },
    Sanitized { packet_corrupt_before: u32, packet_corrupt_after: u32 },
    DiagnosticSanitized { packet_corrupt_before: u32, packet_corrupt_after: u32 },
    Timeout,
    SanitizeFailed { reason: String },
    ValidationFailed { reason: String },
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct HlsWatchdogSanitizeArtifactKey {
    raw_sha256: String,
    command_version: u32,
    ffmpeg_version: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct HlsWatchdogArtifactMetadata {
    status: HlsCorruptSegmentWatchdogStatus,
    raw_size: u64,
    final_size: u64,
    validation_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct HlsCorruptSegmentWatchdogStats {
    pub metadata: usize,
    pub locks: usize,
}

#[derive(Debug, Default)]
pub struct HlsCorruptSegmentWatchdogManager {
    metadata: RwLock<HashMap<HlsWatchdogSanitizeArtifactKey, HlsWatchdogArtifactMetadata>>,
    metadata_order: Mutex<VecDeque<HlsWatchdogSanitizeArtifactKey>>,
    locks: Mutex<HashMap<HlsWatchdogSanitizeArtifactKey, Arc<Mutex<()>>>>,
}

impl HlsCorruptSegmentWatchdogManager {
    pub fn new() -> Self { Self::default() }

    pub async fn clear_runtime_state(&self) {
        self.metadata.write().await.clear();
        self.metadata_order.lock().await.clear();
        self.locks.lock().await.clear();
    }

    pub async fn stats(&self) -> HlsCorruptSegmentWatchdogStats {
        HlsCorruptSegmentWatchdogStats {
            metadata: self.metadata.read().await.len(),
            locks: self.locks.lock().await.len(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn process_staged_and_commit<K>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        raw: StagedCacheObject,
        context: &HlsSegmentRepairObjectContext,
        config: &HlsCorruptSegmentWatchdogConfig,
        semaphore: &Arc<Semaphore>,
        raw_sha256: String,
        deadline: &HlsPostProcessingDeadline,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        if !config.mode.is_enabled() {
            return segment_cache.commit_staged(key, raw).await;
        }
        let identity = HlsWatchdogSanitizeArtifactKey {
            raw_sha256: raw_sha256.clone(),
            command_version: WATCHDOG_COMMAND_VERSION,
            ffmpeg_version: ffmpeg_identity_version(),
        };
        if let Some(metadata) = self.metadata(&identity).await {
            if !matches!(
                metadata.status,
                HlsCorruptSegmentWatchdogStatus::Sanitized { .. }
                    | HlsCorruptSegmentWatchdogStatus::DiagnosticSanitized { .. }
            ) {
                debug_watchdog_event(context, config.mode, "metadata hit", Some(status_log_value(&metadata.status)));
                return segment_cache.commit_staged(key, raw).await;
            }
        }
        let Some(remaining) = deadline.remaining() else {
            self.record_metadata(
                identity,
                HlsCorruptSegmentWatchdogStatus::Timeout,
                raw.size,
                raw.size,
                Some("timeout".to_string()),
            )
            .await;
            return segment_cache.commit_staged(key, raw).await;
        };
        let permit = match tokio::time::timeout(remaining, semaphore.acquire()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return segment_cache.commit_staged(key, raw).await,
            Err(_) => {
                self.record_metadata(
                    identity,
                    HlsCorruptSegmentWatchdogStatus::Timeout,
                    raw.size,
                    raw.size,
                    Some("timeout".to_string()),
                )
                .await;
                return segment_cache.commit_staged(key, raw).await;
            }
        };
        let lock = self.lock_for_identity(identity.clone()).await;
        let result = {
            let _permit = permit;
            let _guard = lock.lock().await;
            self.process_locked(segment_cache, key, raw, context, config, identity.clone(), raw_sha256, deadline)
                .await
        };
        self.remove_lock_if_unused(&identity, &lock).await;
        result
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    async fn process_locked<K>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        raw: StagedCacheObject,
        context: &HlsSegmentRepairObjectContext,
        config: &HlsCorruptSegmentWatchdogConfig,
        identity: HlsWatchdogSanitizeArtifactKey,
        raw_sha256: String,
        deadline: &HlsPostProcessingDeadline,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        if let Some(metadata) = self.metadata(&identity).await {
            if !matches!(
                metadata.status,
                HlsCorruptSegmentWatchdogStatus::Sanitized { .. }
                    | HlsCorruptSegmentWatchdogStatus::DiagnosticSanitized { .. }
            ) {
                return segment_cache.commit_staged(key, raw).await;
            }
        }
        let raw_detect = match detect_packet_corrupt(&raw.path, deadline).await {
            Ok(count) => count,
            Err(reason) if reason == "timeout" => {
                self.record_metadata(
                    identity,
                    HlsCorruptSegmentWatchdogStatus::Timeout,
                    raw.size,
                    raw.size,
                    Some(reason),
                )
                .await;
                return segment_cache.commit_staged(key, raw).await;
            }
            Err(reason) => {
                self.record_metadata(
                    identity,
                    HlsCorruptSegmentWatchdogStatus::ValidationFailed { reason: reason.clone() },
                    raw.size,
                    raw.size,
                    Some(reason),
                )
                .await;
                return segment_cache.commit_staged(key, raw).await;
            }
        };
        if raw_detect == 0 {
            self.record_metadata(identity, HlsCorruptSegmentWatchdogStatus::Clean, raw.size, raw.size, None)
                .await;
            debug_watchdog_event(context, config.mode, "clean", Some("packet_corrupt=0"));
            return segment_cache.commit_staged(key, raw).await;
        }
        if config.mode == HlsCorruptSegmentWatchdogMode::DetectOnly {
            self.record_metadata(
                identity,
                HlsCorruptSegmentWatchdogStatus::DetectedCorrupt {
                    packet_corrupt_count: raw_detect,
                },
                raw.size,
                raw.size,
                None,
            )
            .await;
            debug_watchdog_event(context, config.mode, "detected corrupt", Some("action=raw_commit"));
            return segment_cache.commit_staged(key, raw).await;
        }
        let fixed_path = watchdog_output_path(&raw.path);
        if let Err(reason) = sanitize_corrupt_segment(&raw.path, &fixed_path, deadline).await {
            let _ = fs::remove_file(&fixed_path).await;
            let status = if reason == "timeout" {
                HlsCorruptSegmentWatchdogStatus::Timeout
            } else {
                HlsCorruptSegmentWatchdogStatus::SanitizeFailed { reason: reason.clone() }
            };
            self.record_metadata(identity, status, raw.size, raw.size, Some(reason)).await;
            return segment_cache.commit_staged(key, raw).await;
        }
        let fixed_size = match fs::metadata(&fixed_path).await {
            Ok(metadata) => metadata.len(),
            Err(err) => {
                let reason = format!("metadata_failed:{err}");
                let _ = fs::remove_file(&fixed_path).await;
                self.record_metadata(
                    identity,
                    HlsCorruptSegmentWatchdogStatus::ValidationFailed { reason: reason.clone() },
                    raw.size,
                    raw.size,
                    Some(reason),
                )
                .await;
                return segment_cache.commit_staged(key, raw).await;
            }
        };
        let fixed_detect = match detect_packet_corrupt(&fixed_path, deadline).await {
            Ok(count) => count,
            Err(reason) if reason == "timeout" => {
                let _ = fs::remove_file(&fixed_path).await;
                self.record_metadata(
                    identity,
                    HlsCorruptSegmentWatchdogStatus::Timeout,
                    raw.size,
                    raw.size,
                    Some(reason),
                )
                .await;
                return segment_cache.commit_staged(key, raw).await;
            }
            Err(reason) => {
                let _ = fs::remove_file(&fixed_path).await;
                self.record_metadata(
                    identity,
                    HlsCorruptSegmentWatchdogStatus::ValidationFailed { reason: reason.clone() },
                    raw.size,
                    raw.size,
                    Some(reason),
                )
                .await;
                return segment_cache.commit_staged(key, raw).await;
            }
        };
        let validation = validate_sanitized_segment(&raw.path, raw.size, &fixed_path, fixed_size, fixed_detect, config.mode, deadline).await;
        if let Err(reason) = validation {
            let _ = fs::remove_file(&fixed_path).await;
            self.record_metadata(
                identity,
                HlsCorruptSegmentWatchdogStatus::ValidationFailed { reason: reason.clone() },
                raw.size,
                raw.size,
                Some(reason),
            )
            .await;
            return segment_cache.commit_staged(key, raw).await;
        }
        let _ = segment_cache.remove_staged(raw.clone()).await;
        let committed = segment_cache
            .commit_staged(
                key,
                StagedCacheObject {
                    path: fixed_path,
                    size: fixed_size,
                },
            )
            .await?;
        let status = if config.mode == HlsCorruptSegmentWatchdogMode::Diagnostic {
            HlsCorruptSegmentWatchdogStatus::DiagnosticSanitized {
                packet_corrupt_before: raw_detect,
                packet_corrupt_after: fixed_detect,
            }
        } else {
            HlsCorruptSegmentWatchdogStatus::Sanitized {
                packet_corrupt_before: raw_detect,
                packet_corrupt_after: fixed_detect,
            }
        };
        let committed_sha = sha256_file(&committed.path).await.unwrap_or(raw_sha256);
        self.record_metadata(identity, status, raw.size, committed.size, Some(committed_sha))
            .await;
        debug_watchdog_event(context, config.mode, "sanitized", Some("action=fixed_commit"));
        Ok(committed)
    }

    async fn metadata(&self, identity: &HlsWatchdogSanitizeArtifactKey) -> Option<HlsWatchdogArtifactMetadata> {
        self.metadata.read().await.get(identity).cloned()
    }

    async fn record_metadata(
        &self,
        identity: HlsWatchdogSanitizeArtifactKey,
        status: HlsCorruptSegmentWatchdogStatus,
        raw_size: u64,
        final_size: u64,
        validation_reason: Option<String>,
    ) {
        let inserted_new = {
            let mut metadata = self.metadata.write().await;
            let inserted_new = !metadata.contains_key(&identity);
            metadata.insert(
                identity.clone(),
                HlsWatchdogArtifactMetadata {
                    status,
                    raw_size,
                    final_size,
                    validation_reason,
                },
            );
            inserted_new
        };
        if inserted_new {
            self.metadata_order.lock().await.push_back(identity);
        }
        self.prune_metadata().await;
    }

    async fn lock_for_identity(&self, identity: HlsWatchdogSanitizeArtifactKey) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        Arc::clone(locks.entry(identity).or_insert_with(|| Arc::new(Mutex::new(()))))
    }

    async fn remove_lock_if_unused(&self, identity: &HlsWatchdogSanitizeArtifactKey, lock: &Arc<Mutex<()>>) {
        let mut locks = self.locks.lock().await;
        if Arc::strong_count(lock) <= 2
            && locks
                .get(identity)
                .is_some_and(|current| Arc::ptr_eq(current, lock))
        {
            locks.remove(identity);
        }
    }

    async fn prune_metadata(&self) {
        loop {
            let should_prune = self.metadata.read().await.len() > WATCHDOG_METADATA_MAX_ENTRIES;
            if !should_prune {
                return;
            }
            let Some(oldest) = self.metadata_order.lock().await.pop_front() else {
                return;
            };
            self.metadata.write().await.remove(&oldest);
        }
    }
}

async fn detect_packet_corrupt(path: &Path, deadline: &HlsPostProcessingDeadline) -> Result<u32, String> {
    let path = path.to_str().ok_or_else(|| "invalid_path".to_string())?;
    let output = match run_command_with_deadline(
        "ffmpeg",
        &[
            "-hide_banner",
            "-nostdin",
            "-v",
            "warning",
            "-i",
            path,
            "-map",
            "0",
            "-c",
            "copy",
            "-f",
            "null",
            "-",
        ],
        deadline,
    )
    .await
    {
        Ok(output) => output,
        Err(reason) if reason == "timeout" => return Err(reason),
        Err(stderr) => stderr,
    };
    Ok(count_packet_corrupt_events(&output))
}

fn count_packet_corrupt_events(stderr: &str) -> u32 {
    let mut dts_values = std::collections::HashSet::new();
    let mut count = 0_u32;
    let mut last_increment = 0_u32;
    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(repeated) = parse_repeated_count(trimmed) {
            count = count.saturating_add(last_increment.saturating_mul(repeated));
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if !lower.contains("packet corrupt") {
            last_increment = 0;
            continue;
        }
        if let Some(dts) = parse_packet_corrupt_dts(trimmed) {
            if dts_values.insert(dts) {
                count = count.saturating_add(1);
                last_increment = 1;
            } else {
                last_increment = 0;
            }
        } else {
            count = count.saturating_add(1);
            last_increment = 1;
        }
    }
    count
}

fn parse_repeated_count(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("Last message repeated ")?;
    rest.strip_suffix(" times")?.parse().ok()
}

fn parse_packet_corrupt_dts(line: &str) -> Option<i64> {
    let (_, rest) = line.split_once("dts = ")?;
    rest.split(|ch: char| !ch.is_ascii_digit() && ch != '-')
        .next()
        .filter(|value| !value.is_empty())?
        .parse()
        .ok()
}

async fn sanitize_corrupt_segment(
    input_path: &Path,
    output_path: &Path,
    deadline: &HlsPostProcessingDeadline,
) -> Result<(), String> {
    let input = input_path.to_str().ok_or_else(|| "invalid_input_path".to_string())?;
    let output = output_path.to_str().ok_or_else(|| "invalid_output_path".to_string())?;
    run_command_with_deadline(
        "ffmpeg",
        &[
            "-hide_banner",
            "-nostdin",
            "-y",
            "-fflags",
            "+discardcorrupt",
            "-copyts",
            "-i",
            input,
            "-map",
            "0",
            "-c",
            "copy",
            "-mpegts_flags",
            "+resend_headers",
            "-mpegts_copyts",
            "1",
            "-muxpreload",
            "0",
            "-muxdelay",
            "0",
            "-f",
            "mpegts",
            output,
        ],
        deadline,
    )
    .await
    .map(|_| ())
}

async fn validate_sanitized_segment(
    raw_path: &Path,
    raw_size: u64,
    fixed_path: &Path,
    fixed_size: u64,
    fixed_packet_corrupt: u32,
    mode: HlsCorruptSegmentWatchdogMode,
    deadline: &HlsPostProcessingDeadline,
) -> Result<(), String> {
    if fixed_packet_corrupt > 0 {
        return Err("packet_corrupt_after_sanitize".to_string());
    }
    if fixed_size == 0 {
        return Err("fixed_empty".to_string());
    }
    if size_increase_percent(raw_size, fixed_size) > 2 {
        return Err("fixed_size_increase".to_string());
    }
    if mode == HlsCorruptSegmentWatchdogMode::Diagnostic {
        validate_diagnostic_metadata(raw_path, fixed_path, deadline).await?;
    }
    Ok(())
}

fn size_increase_percent(raw_size: u64, fixed_size: u64) -> u64 {
    if fixed_size <= raw_size {
        return 0;
    }
    if raw_size == 0 {
        return 100;
    }
    fixed_size.saturating_sub(raw_size).saturating_mul(100) / raw_size
}

#[derive(Debug, Clone, Default)]
struct WatchdogProbeMetadata {
    duration_ms: Option<i64>,
    stream_count: usize,
    primary_video_codec: Option<String>,
    primary_audio_codec: Option<String>,
    primary_video_start_time_ms: Option<i64>,
    primary_audio_start_time_ms: Option<i64>,
}

async fn validate_diagnostic_metadata(
    raw_path: &Path,
    fixed_path: &Path,
    deadline: &HlsPostProcessingDeadline,
) -> Result<(), String> {
    let raw = probe_metadata(raw_path, deadline).await?;
    let fixed = probe_metadata(fixed_path, deadline).await?;
    if raw.stream_count != fixed.stream_count {
        return Err("stream_count_changed".to_string());
    }
    if raw.primary_video_codec != fixed.primary_video_codec {
        return Err("primary_video_codec_changed".to_string());
    }
    if raw.primary_audio_codec != fixed.primary_audio_codec {
        return Err("primary_audio_codec_changed".to_string());
    }
    if delta_too_large(raw.primary_video_start_time_ms, fixed.primary_video_start_time_ms, 250)
        || delta_too_large(raw.primary_audio_start_time_ms, fixed.primary_audio_start_time_ms, 250)
    {
        return Err("start_time_delta".to_string());
    }
    if delta_too_large(raw.duration_ms, fixed.duration_ms, 500) {
        return Err("duration_delta".to_string());
    }
    Ok(())
}

fn delta_too_large(left: Option<i64>, right: Option<i64>, max_delta_ms: i64) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.saturating_sub(right).abs() > max_delta_ms,
        _ => false,
    }
}

async fn probe_metadata(path: &Path, deadline: &HlsPostProcessingDeadline) -> Result<WatchdogProbeMetadata, String> {
    let path = path.to_str().ok_or_else(|| "invalid_path".to_string())?;
    let output = run_command_with_deadline(
        "ffprobe",
        &[
            "-hide_banner",
            "-v",
            "error",
            "-show_entries",
            "format=duration,size,bit_rate",
            "-show_entries",
            "stream=index,codec_type,codec_name,start_time,duration,id",
            "-of",
            "json",
            path,
        ],
        deadline,
    )
    .await?;
    parse_probe_metadata(&output)
}

fn parse_probe_metadata(output: &str) -> Result<WatchdogProbeMetadata, String> {
    let value: Value = serde_json::from_str(output).map_err(|err| err.to_string())?;
    let streams = value
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing_streams".to_string())?;
    let mut metadata = WatchdogProbeMetadata {
        stream_count: streams.len(),
        ..WatchdogProbeMetadata::default()
    };
    for stream in streams {
        let codec_type = stream.get("codec_type").and_then(Value::as_str);
        let codec_name = stream.get("codec_name").and_then(Value::as_str).map(str::to_string);
        let start_time_ms = stream
            .get("start_time")
            .and_then(Value::as_str)
            .and_then(parse_seconds_to_millis);
        match codec_type {
            Some("video") if metadata.primary_video_codec.is_none() => {
                metadata.primary_video_codec = codec_name;
                metadata.primary_video_start_time_ms = start_time_ms;
            }
            Some("audio") if metadata.primary_audio_codec.is_none() => {
                metadata.primary_audio_codec = codec_name;
                metadata.primary_audio_start_time_ms = start_time_ms;
            }
            _ => {}
        }
    }
    metadata.duration_ms = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(Value::as_str)
        .and_then(parse_seconds_to_millis);
    Ok(metadata)
}

#[allow(clippy::cast_possible_truncation)]
fn parse_seconds_to_millis(value: &str) -> Option<i64> {
    let value: f64 = value.parse().ok()?;
    Some((value * 1_000.0).round() as i64)
}

fn watchdog_output_path(input_path: &Path) -> PathBuf {
    let mut value: OsString = input_path.as_os_str().to_os_string();
    value.push(".watchdog.ts");
    PathBuf::from(value)
}

fn status_log_value(status: &HlsCorruptSegmentWatchdogStatus) -> &'static str {
    match status {
        HlsCorruptSegmentWatchdogStatus::Clean => "clean",
        HlsCorruptSegmentWatchdogStatus::DetectedCorrupt { .. } => "detected_corrupt",
        HlsCorruptSegmentWatchdogStatus::Sanitized { .. } => "sanitized",
        HlsCorruptSegmentWatchdogStatus::DiagnosticSanitized { .. } => "diagnostic_sanitized",
        HlsCorruptSegmentWatchdogStatus::Timeout => "timeout",
        HlsCorruptSegmentWatchdogStatus::SanitizeFailed { .. } => "sanitize_failed",
        HlsCorruptSegmentWatchdogStatus::ValidationFailed { .. } => "validation_failed",
    }
}

fn debug_watchdog_event(
    context: &HlsSegmentRepairObjectContext,
    mode: HlsCorruptSegmentWatchdogMode,
    event: &'static str,
    detail: Option<&str>,
) {
    if let Some(detail) = detail {
        debug!(
            "HLS corrupt segment watchdog {event}: session={} source={} resource={} mode={} {detail}",
            super::safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            mode.as_log_value()
        );
    } else {
        debug!(
            "HLS corrupt segment watchdog {event}: session={} source={} resource={} mode={}",
            super::safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            mode.as_log_value()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{count_packet_corrupt_events, parse_packet_corrupt_dts};

    #[test]
    fn packet_corrupt_counter_dedupes_mpegts_and_hls_same_dts() {
        let stderr = r"
[mpegts @ 0x1] Packet corrupt (stream = 0, dts = 5850142200).
[hls @ 0x2] Packet corrupt (stream = 0, dts = 5850142200).
[hls @ 0x2] Packet corrupt (stream = 0, dts = 5851004400).
";

        assert_eq!(count_packet_corrupt_events(stderr), 2);
    }

    #[test]
    fn packet_corrupt_counter_expands_repeated_messages() {
        let stderr = r"
Packet corrupt
Last message repeated 3 times
";

        assert_eq!(count_packet_corrupt_events(stderr), 4);
    }

    #[test]
    fn parses_packet_corrupt_dts() {
        assert_eq!(
            parse_packet_corrupt_dts("[mpegts @ 0x1] Packet corrupt (stream = 0, dts = -42)."),
            Some(-42)
        );
    }
}
