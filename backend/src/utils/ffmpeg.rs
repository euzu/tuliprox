use crate::model::ProxyConfig;
use log::{debug, warn};
use reqwest::{
    header::{CONTENT_RANGE, RANGE, USER_AGENT},
    Client, StatusCode,
};
use serde_json::Value;
use shared::model::MediaQuality;
use shared::utils::{default_thumbnail_height, default_thumbnail_width, is_dash_url, is_hls_url, sanitize_sensitive_info};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt},
    process::Command,
};
use url::Url;

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(60);
const FFPROBE_SEEKABLE_MAX_WINDOW_BYTES: u64 = 32 * 1024 * 1024;
const FFPROBE_TEMP_STALE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailureKind {
    NotFound,
    Cancelled,
    Other,
}

pub enum ProbeUrlOutcome {
    Success(MediaQuality, Option<Value>, Option<Value>, ProbeStreamStats),
    Failed(ProbeFailureKind),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeStreamStats {
    pub duration_secs: Option<u32>,
    pub bitrate: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FfmpegExecutor;

#[derive(Debug, Clone, Copy)]
struct SeekableProbeStage {
    content_length: Option<u64>,
    head_bytes: u64,
    tail_start: Option<u64>,
    fully_materialized: bool,
    range_supported: bool,
}

#[derive(Debug, Clone, Copy)]
struct SpanWriteResult {
    status: StatusCode,
    total_length: Option<u64>,
    bytes_written: u64,
    response_exhausted: bool,
}

impl FfmpegExecutor {
    #[must_use]
    pub const fn new() -> Self { Self }

    /// Checks if the system `ffmpeg` binary is available.
    pub async fn check_ffmpeg_availability(&self) -> bool {
        self.check_binary_availability("ffmpeg").await
    }

    // Checks if ffprobe is available in the system path
    pub async fn check_ffprobe_availability(&self) -> bool {
        self.check_binary_availability("ffprobe").await
    }

    /// Extracts a JPEG thumbnail from a local file.
    /// Attempts a frame at 180s first and falls back to 0s for short videos.
    pub async fn create_thumbnail(&self, input_path: &str, width: u32, height: u32) -> Result<Vec<u8>, String> {
        let temp_dir = tempfile::tempdir()
            .map_err(|e| format!("Failed to create temp dir: {e}"))?;
        let output_path = temp_dir.path().join("thumb.jpg");
        let scale_filter = build_thumbnail_scale_filter(width, height);

        let output = self.run_ffmpeg_with_timeout(&build_thumbnail_args(input_path, &output_path, &scale_filter, 180))
            .await
            .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("Output file is empty") || stderr.contains("nothing was encoded") {
                debug!("Video shorter than 180s, retrying at position 0: {input_path}");
                let retry = self.run_ffmpeg_with_timeout(&build_thumbnail_args(input_path, &output_path, &scale_filter, 0))
                    .await
                    .map_err(|e| format!("Failed to run ffmpeg retry: {e}"))?;

                if !retry.status.success() {
                    let retry_stderr = String::from_utf8_lossy(&retry.stderr);
                    return Err(format!("ffmpeg failed at position 0: {retry_stderr}"));
                }
            } else {
                return Err(format!("ffmpeg failed: {stderr}"));
            }
        }

        tokio::fs::read(&output_path)
            .await
            .map_err(|e| format!("Failed to read thumbnail: {e}"))
    }

    pub async fn probe_url(
        &self,
        url: &str,
        user_agent: Option<&str>,
        analyze_duration: u64,
        probe_size: u64,
        timeout_secs: u64,
        proxy_cfg: Option<&ProxyConfig>,
    ) -> ProbeUrlOutcome {
        // Determine timeout: Ensure it's at least as long as the analyze duration + buffer,
        // but respect the user setting if it's longer.
        let analyze_overhead = Duration::from_micros(analyze_duration) + Duration::from_secs(5);
        let config_timeout = Duration::from_secs(timeout_secs);
        let timeout_val = std::cmp::max(analyze_overhead, config_timeout);

        let mut command = Command::new("ffprobe");

        // Ensure the child process is killed if this future is dropped (e.g. by connection preemption)
        command.kill_on_drop(true);

        command
            .arg("-v").arg("error")
            .arg("-show_streams")
            .arg("-show_format")
            .arg("-of").arg("json")
            .arg("-analyzeduration").arg(analyze_duration.to_string())
            .arg("-probesize").arg(probe_size.to_string());

        apply_proxy_to_ffprobe(&mut command, proxy_cfg);

        if let Some(ua) = user_agent {
            command.arg("-user_agent").arg(ua);
        }

        command.arg(url);

        let output_result = tokio::time::timeout(timeout_val, command.output()).await;

        match output_result {
            Ok(Ok(output)) => return parse_ffprobe_output(url, &output),
            Ok(Err(e)) => {
                warn!("ffprobe execution failed for {}: {}", sanitize_sensitive_info(url), e);
            }
            Err(_) => {
                warn!("ffprobe timed out after {:?} for {}", timeout_val, sanitize_sensitive_info(url));
            }
        }

        ProbeUrlOutcome::Failed(ProbeFailureKind::Other)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn probe_remote_url(
        &self,
        client: &Client,
        url: &str,
        user_agent: Option<&str>,
        analyze_duration: u64,
        probe_size: u64,
        timeout_secs: u64,
    ) -> ProbeUrlOutcome {
        let analyze_overhead = Duration::from_micros(analyze_duration) + Duration::from_secs(5);
        let config_timeout = Duration::from_secs(timeout_secs);
        let timeout_val = std::cmp::max(analyze_overhead, config_timeout);

        let probe_result = tokio::time::timeout(
            timeout_val,
            self.probe_remote_url_inner(client, url, user_agent, analyze_duration, probe_size),
        )
        .await;

        if let Ok(outcome) = probe_result {
            outcome
        } else {
            warn!(
                "ffprobe remote stdin probe timed out after {:?} for {}",
                timeout_val,
                sanitize_sensitive_info(url)
            );
            ProbeUrlOutcome::Failed(ProbeFailureKind::Other)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn probe_remote_seekable_url(
        &self,
        client: &Client,
        url: &str,
        user_agent: Option<&str>,
        analyze_duration: u64,
        probe_size: u64,
        timeout_secs: u64,
    ) -> ProbeUrlOutcome {
        let analyze_overhead = Duration::from_micros(analyze_duration) + Duration::from_secs(5);
        let config_timeout = Duration::from_secs(timeout_secs);
        let timeout_val = std::cmp::max(analyze_overhead, config_timeout);

        let probe_result = tokio::time::timeout(
            timeout_val,
            self.probe_remote_seekable_url_inner(client, url, user_agent, analyze_duration, probe_size),
        )
        .await;

        if let Ok(outcome) = probe_result {
            outcome
        } else {
            warn!(
                "ffprobe remote seekable probe timed out after {:?} for {}",
                timeout_val,
                sanitize_sensitive_info(url)
            );
            ProbeUrlOutcome::Failed(ProbeFailureKind::Other)
        }
    }

    /// Wrapper around [`Self::probe_url`] that races the probe against an optional cancellation token.
    #[allow(clippy::too_many_arguments)]
    pub async fn probe_url_with_cancel(
        &self,
        url: &str,
        user_agent: Option<&str>,
        analyze_duration: u64,
        probe_size: u64,
        timeout_secs: u64,
        proxy_cfg: Option<&ProxyConfig>,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
    ) -> ProbeUrlOutcome {
        if let Some(token) = cancel_token {
            tokio::select! {
                biased;
                () = token.cancelled() => {
                    warn!("Probe preempted for {}", shared::utils::sanitize_sensitive_info(url));
                    ProbeUrlOutcome::Failed(ProbeFailureKind::Cancelled)
                }
                result = self.probe_url(url, user_agent, analyze_duration, probe_size, timeout_secs, proxy_cfg) => result,
            }
        } else {
            self.probe_url(url, user_agent, analyze_duration, probe_size, timeout_secs, proxy_cfg).await
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn probe_remote_url_with_cancel(
        &self,
        client: &Client,
        url: &str,
        user_agent: Option<&str>,
        analyze_duration: u64,
        probe_size: u64,
        timeout_secs: u64,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
    ) -> ProbeUrlOutcome {
        if let Some(token) = cancel_token {
            tokio::select! {
                biased;
                () = token.cancelled() => {
                    warn!("Probe preempted for {}", shared::utils::sanitize_sensitive_info(url));
                    ProbeUrlOutcome::Failed(ProbeFailureKind::Cancelled)
                }
                result = self.probe_remote_url(client, url, user_agent, analyze_duration, probe_size, timeout_secs) => result,
            }
        } else {
            self.probe_remote_url(client, url, user_agent, analyze_duration, probe_size, timeout_secs).await
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn probe_remote_seekable_url_with_cancel(
        &self,
        client: &Client,
        url: &str,
        user_agent: Option<&str>,
        analyze_duration: u64,
        probe_size: u64,
        timeout_secs: u64,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
    ) -> ProbeUrlOutcome {
        if let Some(token) = cancel_token {
            tokio::select! {
                biased;
                () = token.cancelled() => {
                    warn!("Probe preempted for {}", shared::utils::sanitize_sensitive_info(url));
                    ProbeUrlOutcome::Failed(ProbeFailureKind::Cancelled)
                }
                result = self.probe_remote_seekable_url(client, url, user_agent, analyze_duration, probe_size, timeout_secs) => result,
            }
        } else {
            self.probe_remote_seekable_url(client, url, user_agent, analyze_duration, probe_size, timeout_secs).await
        }
    }

    async fn check_binary_availability(&self, binary: &str) -> bool {
        let mut command = Command::new(binary);
        command
            .arg("-version")
            .kill_on_drop(true);

        match tokio::time::timeout(FFMPEG_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output.status.success(),
            Ok(Err(_)) | Err(_) => false,
        }
    }

    async fn run_ffmpeg_with_timeout(&self, args: &[String]) -> Result<std::process::Output, String> {
        let child = Command::new("ffmpeg")
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;

        tokio::time::timeout(FFMPEG_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| format_ffmpeg_timeout_error(args))?
            .map_err(|e| e.to_string())
    }

    async fn probe_remote_url_inner(
        &self,
        client: &Client,
        url: &str,
        user_agent: Option<&str>,
        analyze_duration: u64,
        probe_size: u64,
    ) -> ProbeUrlOutcome {
        let mut command = Command::new("ffprobe");
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("-v")
            .arg("error")
            .arg("-show_streams")
            .arg("-show_format")
            .arg("-of")
            .arg("json")
            .arg("-analyzeduration")
            .arg(analyze_duration.to_string())
            .arg("-probesize")
            .arg(probe_size.to_string())
            .arg("-i")
            .arg("pipe:0");

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                warn!("ffprobe execution failed for {}: {}", sanitize_sensitive_info(url), err);
                return ProbeUrlOutcome::Failed(ProbeFailureKind::Other);
            }
        };

        let Some(mut stdin) = child.stdin.take() else {
            warn!("ffprobe stdin unavailable for {}", sanitize_sensitive_info(url));
            return ProbeUrlOutcome::Failed(ProbeFailureKind::Other);
        };

        let max_bytes = if probe_size == 0 { usize::MAX } else { usize::try_from(probe_size).unwrap_or(usize::MAX) };
        let fetch_result = stream_probe_bytes_to_stdin(&mut stdin, client, url, user_agent, probe_size, max_bytes).await;

        let stdin_err = match fetch_result {
            Ok(()) => None,
            Err(ProbeFailureKind::NotFound) => {
                let _ = child.kill().await;
                return ProbeUrlOutcome::Failed(ProbeFailureKind::NotFound);
            }
            Err(kind) => Some(kind),
        };

        if let Err(err) = stdin.shutdown().await {
            if err.kind() != ErrorKind::BrokenPipe {
                debug!(
                    "ffprobe stdin shutdown reported {} for {}, continuing with child output",
                    err,
                    sanitize_sensitive_info(url)
                );
            }
        }
        drop(stdin);

        match child.wait_with_output().await {
            Ok(output) => {
                if let Some(kind) = stdin_err {
                    debug!("ffprobe remote stdin probe had fetch error, checking child output for {}", sanitize_sensitive_info(url));
                    let outcome = parse_ffprobe_output(url, &output);
                    if matches!(outcome, ProbeUrlOutcome::Success(..)) {
                        return outcome;
                    }
                    ProbeUrlOutcome::Failed(kind)
                } else {
                    parse_ffprobe_output(url, &output)
                }
            }
            Err(err) => {
                warn!("ffprobe execution failed for {}: {}", sanitize_sensitive_info(url), err);
                ProbeUrlOutcome::Failed(stdin_err.unwrap_or(ProbeFailureKind::Other))
            }
        }
    }

    async fn probe_remote_seekable_url_inner(
        &self,
        client: &Client,
        url: &str,
        user_agent: Option<&str>,
        analyze_duration: u64,
        probe_size: u64,
    ) -> ProbeUrlOutcome {
        let temp_file = match create_seekable_probe_temp_file().await {
            Ok(temp_file) => temp_file,
            Err(kind) => return ProbeUrlOutcome::Failed(kind),
        };
        let temp_path = temp_file.path().to_path_buf();

        let stage = match stage_seekable_probe_file(client, url, user_agent, probe_size, &temp_path).await {
            Ok(stage) => stage,
            Err(kind) => return ProbeUrlOutcome::Failed(kind),
        };

        if !stage.fully_materialized {
            debug!(
                "Running seekable ffprobe probe with staged head/tail data for {}",
                sanitize_sensitive_info(url)
            );
        }
        probe_local_path_with_ffprobe(&temp_path, analyze_duration, probe_size).await
    }
}

#[must_use]
pub fn is_supported_probe_url(url: &str) -> bool {
    // TODO: HLS manifests are intentionally unsupported for ffprobe-based metadata extraction for now.
    // Probing them correctly requires explicit variant selection and base-url aware follow-up fetching,
    // which we do not model yet. Revisit this once HLS probing semantics are defined.
    // DASH manifests are also excluded for the same reason.
    !is_hls_url(url) && !is_dash_url(url)
}

fn format_ffmpeg_timeout_error(args: &[String]) -> String {
    let summary = args.join(" ");
    format!(
        "Timed out running ffmpeg after {}s: {summary}",
        FFMPEG_TIMEOUT.as_secs()
    )
}

fn probe_bytes_limit(probe_size: u64) -> usize {
    if probe_size == 0 {
        usize::MAX
    } else {
        usize::try_from(probe_size).unwrap_or(usize::MAX)
    }
}

fn seekable_probe_window_bytes(probe_size: u64) -> u64 {
    if probe_size == 0 {
        FFPROBE_SEEKABLE_MAX_WINDOW_BYTES
    } else {
        std::cmp::min(probe_size.max(1), FFPROBE_SEEKABLE_MAX_WINDOW_BYTES)
    }
}

fn seekable_probe_temp_dir() -> PathBuf { std::env::temp_dir().join("ffprobe") }

async fn create_seekable_probe_temp_file() -> Result<tempfile::NamedTempFile, ProbeFailureKind> {
    let temp_dir = seekable_probe_temp_dir();
    if let Err(err) = fs::create_dir_all(&temp_dir).await {
        warn!("Failed to create ffprobe temp dir {}: {}", temp_dir.display(), err);
        return Err(ProbeFailureKind::Other);
    }
    if let Err(err) = cleanup_stale_seekable_probe_files(&temp_dir).await {
        debug!(
            "Failed to cleanup stale ffprobe temp files in {}: {}",
            temp_dir.display(),
            err
        );
    }

    tempfile::Builder::new()
        .prefix("probe-")
        .suffix(".bin")
        .tempfile_in(&temp_dir)
        .map_err(|err| {
            warn!("Failed to create ffprobe temp file in {}: {}", temp_dir.display(), err);
            ProbeFailureKind::Other
        })
}

async fn cleanup_stale_seekable_probe_files(temp_dir: &Path) -> std::io::Result<()> {
    let mut entries = match fs::read_dir(temp_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !file_name.starts_with("probe-") {
            continue;
        }

        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(modified_elapsed) = modified.elapsed() else {
            continue;
        };
        if modified_elapsed <= FFPROBE_TEMP_STALE_MAX_AGE {
            continue;
        }

        if let Err(err) = fs::remove_file(&path).await {
            debug!("Failed to remove stale ffprobe temp file {}: {}", path.display(), err);
        }
    }

    Ok(())
}

async fn ensure_probe_file_length(path: &Path, content_length: Option<u64>) -> Result<(), ProbeFailureKind> {
    let Some(content_length) = content_length else {
        return Ok(());
    };
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .await
        .map_err(|err| {
            warn!(
                "Failed to open seekable ffprobe temp file {}: {}",
                path.display(),
                err
            );
            ProbeFailureKind::Other
        })?;
    file.set_len(content_length).await.map_err(|err| {
        warn!(
            "Failed to resize seekable ffprobe temp file {}: {}",
            path.display(),
            err
        );
        ProbeFailureKind::Other
    })
}

async fn probe_local_path_with_ffprobe(path: &Path, analyze_duration: u64, probe_size: u64) -> ProbeUrlOutcome {
    let display_path = path.to_string_lossy().into_owned();
    let mut command = Command::new("ffprobe");
    command
        .kill_on_drop(true)
        .arg("-v")
        .arg("error")
        .arg("-show_streams")
        .arg("-show_format")
        .arg("-of")
        .arg("json")
        .arg("-analyzeduration")
        .arg(analyze_duration.to_string())
        .arg("-probesize")
        .arg(probe_size.to_string())
        .arg(path);

    match command.output().await {
        Ok(output) => parse_ffprobe_output(&display_path, &output),
        Err(err) => {
            warn!(
                "ffprobe execution failed for {}: {}",
                sanitize_sensitive_info(&display_path),
                err
            );
            ProbeUrlOutcome::Failed(ProbeFailureKind::Other)
        }
    }
}

async fn stage_seekable_probe_file(
    client: &Client,
    url: &str,
    user_agent: Option<&str>,
    probe_size: u64,
    temp_path: &Path,
) -> Result<SeekableProbeStage, ProbeFailureKind> {
    let head_window = seekable_probe_window_bytes(probe_size);
    let tail_window = seekable_probe_window_bytes(probe_size);
    let head_result = fetch_remote_span_to_file(
        client,
        url,
        user_agent,
        Some((0, head_window.saturating_sub(1))),
        temp_path,
        0,
        probe_bytes_limit(head_window),
        true,
    )
    .await?;

    ensure_probe_file_length(temp_path, head_result.total_length).await?;

    let mut stage = SeekableProbeStage {
        content_length: head_result.total_length,
        head_bytes: head_result.bytes_written,
        tail_start: None,
        fully_materialized: head_result.status != StatusCode::PARTIAL_CONTENT
            && head_result.response_exhausted
            && head_result
                .total_length
                .is_none_or(|content_length| head_result.bytes_written >= content_length),
        range_supported: head_result.status == StatusCode::PARTIAL_CONTENT,
    };

    if stage.fully_materialized || !stage.range_supported {
        if stage.fully_materialized {
            debug!(
                "Seekable ffprobe probe fully materialized from the first pass for {}",
                sanitize_sensitive_info(url)
            );
        }
        return Ok(stage);
    }

    let Some(content_length) = stage.content_length else {
        return Ok(stage);
    };
    if content_length <= stage.head_bytes {
        stage.fully_materialized = true;
        return Ok(stage);
    }

    let tail_start = content_length.saturating_sub(tail_window).max(stage.head_bytes);
    if tail_start >= content_length {
        return Ok(stage);
    }

    debug!(
        "Staging seekable ffprobe head/tail windows for {} (head={} bytes, tail_start={})",
        sanitize_sensitive_info(url),
        stage.head_bytes,
        tail_start
    );

    let tail_result = fetch_remote_span_to_file(
        client,
        url,
        user_agent,
        Some((tail_start, content_length - 1)),
        temp_path,
        tail_start,
        probe_bytes_limit(content_length - tail_start),
        false,
    )
    .await?;

    if tail_result.status == StatusCode::PARTIAL_CONTENT && tail_result.bytes_written > 0 {
        stage.tail_start = Some(tail_start);
        if tail_start == stage.head_bytes && tail_result.bytes_written == content_length - tail_start {
            stage.fully_materialized = true;
        }
    } else {
        debug!(
            "Remote server ignored seekable ffprobe tail range for {}, falling back without tail staging",
            sanitize_sensitive_info(url)
        );
        stage.range_supported = false;
    }

    Ok(stage)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_remote_span_to_file(
    client: &Client,
    url: &str,
    user_agent: Option<&str>,
    range: Option<(u64, u64)>,
    path: &Path,
    offset: u64,
    max_bytes: usize,
    allow_full_body_write_on_200: bool,
) -> Result<SpanWriteResult, ProbeFailureKind> {
    let mut request = client.get(url);
    if let Some(ua) = user_agent {
        request = request.header(USER_AGENT, ua);
    }
    if let Some((start, end)) = range {
        request = request.header(RANGE, format!("bytes={start}-{end}"));
    }

    let mut response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!("ffprobe fetch failed for {}: {}", sanitize_sensitive_info(url), err);
            return Err(ProbeFailureKind::Other);
        }
    };

    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(ProbeFailureKind::NotFound);
    }
    if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
        warn!(
            "ffprobe fetch returned {} for {}",
            status,
            sanitize_sensitive_info(url)
        );
        return Err(ProbeFailureKind::Other);
    }

    let total_length = parse_remote_content_length(&response);
    if status == StatusCode::OK && range.is_some() && !allow_full_body_write_on_200 {
        return Ok(SpanWriteResult {
            status,
            total_length,
            bytes_written: 0,
            response_exhausted: false,
        });
    }

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .await
        .map_err(|err| {
            warn!(
                "Failed to open seekable ffprobe temp file {}: {}",
                path.display(),
                err
            );
            ProbeFailureKind::Other
        })?;
    file.seek(std::io::SeekFrom::Start(offset)).await.map_err(|err| {
        warn!(
            "Failed to seek seekable ffprobe temp file {}: {}",
            path.display(),
            err
        );
        ProbeFailureKind::Other
    })?;

    let mut bytes_written: usize = 0;
    let mut response_exhausted = false;
    while bytes_written < max_bytes {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                response_exhausted = true;
                break;
            }
            Err(err) => {
                warn!("ffprobe fetch body failed for {}: {}", sanitize_sensitive_info(url), err);
                return Err(ProbeFailureKind::Other);
            }
        };

        let remaining = max_bytes.saturating_sub(bytes_written);
        let to_write = chunk.len().min(remaining);
        if to_write == 0 {
            break;
        }
        file.write_all(&chunk[..to_write]).await.map_err(|err| {
            warn!(
                "Failed to write seekable ffprobe temp file {}: {}",
                path.display(),
                err
            );
            ProbeFailureKind::Other
        })?;
        bytes_written += to_write;

        if bytes_written >= max_bytes {
            break;
        }
    }

    Ok(SpanWriteResult {
        status,
        total_length,
        bytes_written: u64::try_from(bytes_written).unwrap_or(u64::MAX),
        response_exhausted,
    })
}

fn parse_remote_content_length(response: &reqwest::Response) -> Option<u64> {
    if response.status() == StatusCode::PARTIAL_CONTENT {
        return response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range_total_length);
    }
    response.content_length()
}

fn parse_content_range_total_length(header_value: &str) -> Option<u64> {
    let (_, total_length) = header_value.rsplit_once('/')?;
    if total_length == "*" {
        None
    } else {
        total_length.parse::<u64>().ok()
    }
}

async fn stream_probe_bytes_to_stdin<W>(
    stdin: &mut W,
    client: &Client,
    url: &str,
    user_agent: Option<&str>,
    probe_size_header: u64,
    max_bytes: usize,
) -> Result<(), ProbeFailureKind>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut request = client.get(url);
    if let Some(ua) = user_agent {
        request = request.header(USER_AGENT, ua);
    }
    if probe_size_header > 0 {
        request = request.header(RANGE, format!("bytes=0-{}", probe_size_header.saturating_sub(1)));
    }

    let mut response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!("ffprobe fetch failed for {}: {}", sanitize_sensitive_info(url), err);
            return Err(ProbeFailureKind::Other);
        }
    };

    if response.status() == StatusCode::NOT_FOUND {
        return Err(ProbeFailureKind::NotFound);
    }
    if !response.status().is_success() && response.status() != StatusCode::PARTIAL_CONTENT {
        warn!(
            "ffprobe fetch returned {} for {}",
            response.status(),
            sanitize_sensitive_info(url)
        );
        return Err(ProbeFailureKind::Other);
    }

    let mut total_written: usize = 0;
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(err) => {
                warn!("ffprobe fetch body failed for {}: {}", sanitize_sensitive_info(url), err);
                return Err(ProbeFailureKind::Other);
            }
        };

        let remaining = max_bytes.saturating_sub(total_written);
        let to_write = if chunk.len() <= remaining {
            chunk.len()
        } else {
            remaining
        };

        if to_write > 0 {
            match stdin.write_all(&chunk[..to_write]).await {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                    debug!(
                        "ffprobe closed stdin early for {}, stopping fetch",
                        sanitize_sensitive_info(url)
                    );
                    return Ok(());
                }
                Err(err) => {
                    warn!("ffprobe stdin write failed for {}: {}", sanitize_sensitive_info(url), err);
                    return Err(ProbeFailureKind::Other);
                }
            }
            total_written += to_write;
        }

        if total_written >= max_bytes {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
async fn fetch_probe_bytes(
    client: &Client,
    url: &str,
    user_agent: Option<&str>,
    probe_size: u64,
) -> Result<Vec<u8>, ProbeFailureKind> {
    let max_bytes = if probe_size == 0 { usize::MAX } else { usize::try_from(probe_size).unwrap_or(usize::MAX) };
    let mut request = client.get(url);
    if let Some(ua) = user_agent {
        request = request.header(USER_AGENT, ua);
    }
    if probe_size > 0 {
        request = request.header(RANGE, format!("bytes=0-{}", probe_size.saturating_sub(1)));
    }

    let mut response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!("ffprobe fetch failed for {}: {}", sanitize_sensitive_info(url), err);
            return Err(ProbeFailureKind::Other);
        }
    };

    if response.status() == StatusCode::NOT_FOUND {
        return Err(ProbeFailureKind::NotFound);
    }
    if !response.status().is_success() && response.status() != StatusCode::PARTIAL_CONTENT {
        warn!(
            "ffprobe fetch returned {} for {}",
            response.status(),
            sanitize_sensitive_info(url)
        );
        return Err(ProbeFailureKind::Other);
    }

    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    while bytes.len() < max_bytes {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(err) => {
                warn!("ffprobe fetch body failed for {}: {}", sanitize_sensitive_info(url), err);
                return Err(ProbeFailureKind::Other);
            }
        };

        let remaining = max_bytes.saturating_sub(bytes.len());
        if chunk.len() <= remaining {
            bytes.extend_from_slice(&chunk);
        } else {
            bytes.extend_from_slice(&chunk[..remaining]);
            break;
        }
    }

    Ok(bytes)
}

#[cfg(test)]
async fn write_probe_bytes_to_stdin<W>(stdin: &mut W, probe_bytes: &[u8], url: &str) -> Result<(), ProbeFailureKind>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match stdin.write_all(probe_bytes).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::BrokenPipe => {
            debug!(
                "ffprobe closed stdin early for {}, continuing with child output",
                sanitize_sensitive_info(url)
            );
            Ok(())
        }
        Err(err) => {
            warn!("ffprobe stdin write failed for {}: {}", sanitize_sensitive_info(url), err);
            Err(ProbeFailureKind::Other)
        }
    }
}

#[cfg(test)]
async fn run_ffprobe_with_stdin(mut child: tokio::process::Child, probe_bytes: &[u8], url: &str) -> ProbeUrlOutcome {
    let Some(mut stdin) = child.stdin.take() else {
        warn!("ffprobe stdin unavailable for {}", sanitize_sensitive_info(url));
        return ProbeUrlOutcome::Failed(ProbeFailureKind::Other);
    };

    if let Err(kind) = write_probe_bytes_to_stdin(&mut stdin, probe_bytes, url).await {
        return ProbeUrlOutcome::Failed(kind);
    }

    if let Err(err) = stdin.shutdown().await {
        if err.kind() != ErrorKind::BrokenPipe {
            debug!(
                "ffprobe stdin shutdown reported {} for {}, continuing with child output",
                err,
                sanitize_sensitive_info(url)
            );
        }
    }
    drop(stdin);

    match child.wait_with_output().await {
        Ok(output) => parse_ffprobe_output(url, &output),
        Err(err) => {
            warn!("ffprobe execution failed for {}: {}", sanitize_sensitive_info(url), err);
            ProbeUrlOutcome::Failed(ProbeFailureKind::Other)
        }
    }
}

fn build_ffprobe_proxy_url(proxy_cfg: &ProxyConfig) -> Option<String> {
    let mut proxy_url = Url::parse(proxy_cfg.url.as_str()).ok()?;
    if let Some(username) = proxy_cfg.username.as_deref() {
        let _ = proxy_url.set_username(username);
        if let Some(password) = proxy_cfg.password.as_deref() {
            let _ = proxy_url.set_password(Some(password));
        }
    }
    Some(proxy_url.to_string())
}

fn apply_proxy_to_ffprobe(command: &mut Command, proxy_cfg: Option<&ProxyConfig>) {
    let Some(proxy_cfg) = proxy_cfg else {
        return;
    };

    let Some(proxy_url) = build_ffprobe_proxy_url(proxy_cfg) else {
        warn!(
            "Ignoring invalid ffprobe proxy URL: {}",
            sanitize_sensitive_info(proxy_cfg.url.as_str())
        );
        return;
    };

    // ffprobe is an external process and does not consume the app's reqwest proxy config.
    // Export proxy env vars explicitly so all probe requests honor the configured upstream proxy.
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(key, proxy_url.as_str());
    }
}

fn parse_ffprobe_output(url: &str, output: &Output) -> ProbeUrlOutcome {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!("ffprobe failed for {}: {}", sanitize_sensitive_info(url), sanitize_sensitive_info(&stderr));
        if is_not_found_probe_error(&stderr) {
            return ProbeUrlOutcome::Failed(ProbeFailureKind::NotFound);
        }
        return ProbeUrlOutcome::Failed(ProbeFailureKind::Other);
    }

    if let Ok(json) = serde_json::from_slice::<Value>(&output.stdout) {
        if let Some(stream_list) = json.get("streams").and_then(Value::as_array) {
            let mut video_stream: Option<&Value> = None;
            let mut audio_stream: Option<&Value> = None;

            for stream in stream_list {
                let codec_type = stream.get("codec_type").and_then(Value::as_str);
                if video_stream.is_none()
                    && (codec_type == Some("video")
                        || (codec_type.is_none() && (stream.get("width").is_some() || stream.get("height").is_some())))
                    && !is_attached_pic(stream)
                {
                    video_stream = Some(stream);
                } else if audio_stream.is_none()
                    && (codec_type == Some("audio")
                        || (codec_type.is_none()
                            && (stream.get("channels").is_some() || stream.get("channel_layout").is_some())))
                {
                    audio_stream = Some(stream);
                }
                if video_stream.is_some() && audio_stream.is_some() {
                    break;
                }
            }

            if video_stream.is_some() || audio_stream.is_some() {
                let video_str = video_stream.map(Value::to_string);
                let audio_str = audio_stream.map(Value::to_string);
                let mq = MediaQuality::from_ffprobe_info(audio_str.as_deref(), video_str.as_deref());
                if let Some(quality) = mq {
                    let stats = extract_probe_stream_stats(&json, video_stream, audio_stream);
                    return ProbeUrlOutcome::Success(quality, video_stream.cloned(), audio_stream.cloned(), stats);
                }
            }
        }
    } else {
        warn!("Failed to parse ffprobe json output for {}", sanitize_sensitive_info(url));
    }

    ProbeUrlOutcome::Failed(ProbeFailureKind::Other)
}

/// Returns `true` when the stream is an embedded thumbnail / cover art
/// (e.g. PNG or MJPEG poster images inside MKV containers).
/// ffprobe reports these as `codec_type: "video"` but with
/// `disposition.attached_pic: 1`.
fn is_attached_pic(stream: &Value) -> bool {
    stream
        .get("disposition")
        .and_then(|d| d.get("attached_pic"))
        .and_then(Value::as_u64)
        == Some(1)
}

fn is_not_found_probe_error(stderr: &str) -> bool {
    let normalized = stderr.to_ascii_lowercase();
    [
        "http error 404",
        "404 not found",
        "http/1.1 404",
        "http/2 404",
        "server returned 404",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn extract_probe_stream_stats(json: &Value, video_stream: Option<&Value>, audio_stream: Option<&Value>) -> ProbeStreamStats {
    let format = json.get("format");
    ProbeStreamStats {
        duration_secs: format
            .and_then(|value| parse_duration_secs(value.get("duration")))
            .or_else(|| parse_duration_secs(video_stream.and_then(|value| value.get("duration"))))
            .or_else(|| parse_duration_secs(audio_stream.and_then(|value| value.get("duration")))),
        bitrate: format
            .and_then(|value| parse_u32_field(value.get("bit_rate")))
            .or_else(|| parse_u32_field(video_stream.and_then(|value| value.get("bit_rate"))))
            .or_else(|| parse_u32_field(audio_stream.and_then(|value| value.get("bit_rate")))),
    }
}

fn parse_duration_secs(value: Option<&Value>) -> Option<u32> {
    let seconds = value.and_then(|value| match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    })?;

    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }

    let rounded = seconds.round();
    if !(1.0..=f64::from(u32::MAX)).contains(&rounded) {
        return None;
    }

    Some(rounded_duration_secs_to_u32(rounded))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rounded_duration_secs_to_u32(rounded_seconds: f64) -> u32 {
    rounded_seconds as u32
}

fn parse_u32_field(value: Option<&Value>) -> Option<u32> {
    let raw = value.and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse::<u64>().ok(),
        _ => None,
    })?;

    u32::try_from(raw).ok().filter(|parsed| *parsed > 0)
}

fn build_thumbnail_scale_filter(width: u32, height: u32) -> String {
    let w = if width < 1 { default_thumbnail_width() } else { width };
    let h = if height < 1 { default_thumbnail_height() } else { height };
    format!(
        "scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}"
    )
}

fn build_thumbnail_args(input_path: &str, output_path: &Path, scale_filter: &str, seek_seconds: u32) -> Vec<String> {
    vec![
        "-ss".to_string(),
        seek_seconds.to_string(),
        "-i".to_string(),
        input_path.to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-vf".to_string(),
        scale_filter.to_string(),
        "-q:v".to_string(),
        "1".to_string(),
        "-y".to_string(),
        output_path.to_string_lossy().into_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        build_ffprobe_proxy_url, build_thumbnail_args, build_thumbnail_scale_filter, extract_probe_stream_stats,
        ensure_probe_file_length, fetch_probe_bytes, format_ffmpeg_timeout_error, parse_content_range_total_length,
        run_ffprobe_with_stdin, seekable_probe_window_bytes, stage_seekable_probe_file, write_probe_bytes_to_stdin,
        FFMPEG_TIMEOUT,
        ProbeFailureKind, ProbeStreamStats, ProbeUrlOutcome,
    };
    use crate::model::ProxyConfig;
    use serde_json::json;
    use shared::utils::{default_thumbnail_height, default_thumbnail_width};
    use std::{io, path::Path, pin::Pin, task::{Context, Poll}};
    use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, Mutex};
    use tokio::{
        io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
        net::TcpListener,
        process::Command,
    };

    async fn start_test_http_server(
        status_line: &str,
        body: &'static [u8],
    ) -> std::io::Result<(String, Arc<Mutex<Vec<u8>>>, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let request_capture = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let request_capture_clone = Arc::clone(&request_capture);
        let accepted_clone = Arc::clone(&accepted);
        let content_length = body.len();
        let status_line = status_line.to_string();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    continue;
                };
                accepted_clone.fetch_add(1, Ordering::SeqCst);
                let request_capture = Arc::clone(&request_capture_clone);
                let status_line = status_line.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 4096];
                    if let Ok(read) = socket.read(&mut buf).await {
                        if read > 0 {
                            let mut guard = request_capture.lock().expect("request capture mutex poisoned");
                            guard.extend_from_slice(&buf[..read]);
                        }
                    }
                    let response_head = format!(
                        "{status_line}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response_head.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Ok((format!("http://127.0.0.1:{}/probe.ts", addr.port()), request_capture, accepted, handle))
    }

    async fn start_range_test_http_server(
        body: Arc<Vec<u8>>,
    ) -> std::io::Result<(String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_clone = Arc::clone(&accepted);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    continue;
                };
                accepted_clone.fetch_add(1, Ordering::SeqCst);
                let body = Arc::clone(&body);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0_u8; 4096];
                    loop {
                        match socket.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(read) => {
                                request.extend_from_slice(&buf[..read]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }

                    let request_text = String::from_utf8_lossy(&request);
                    let range_header = request_text
                        .lines()
                        .find_map(|line| line.strip_prefix("Range: bytes="))
                        .or_else(|| request_text.lines().find_map(|line| line.strip_prefix("range: bytes=")));

                    let (status_line, content_range, payload) = if let Some(range_header) = range_header {
                        let (start, end) = range_header
                            .split_once('-')
                            .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)))
                            .unwrap_or((0, body.len().saturating_sub(1)));
                        let bounded_end = end.min(body.len().saturating_sub(1));
                        let bounded_start = start.min(bounded_end);
                        (
                            "HTTP/1.1 206 Partial Content".to_string(),
                            Some(format!(
                                "Content-Range: bytes {}-{}/{}\r\n",
                                bounded_start,
                                bounded_end,
                                body.len()
                            )),
                            body[bounded_start..=bounded_end].to_vec(),
                        )
                    } else {
                        ("HTTP/1.1 200 OK".to_string(), None, body.as_ref().clone())
                    };

                    let mut response_head = format!(
                        "{status_line}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n",
                        payload.len()
                    );
                    if let Some(content_range) = content_range {
                        response_head.push_str(&content_range);
                    }
                    response_head.push_str("\r\n");

                    let _ = socket.write_all(response_head.as_bytes()).await;
                    let _ = socket.write_all(&payload).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Ok((format!("http://127.0.0.1:{}/probe.mp4", addr.port()), accepted, handle))
    }

    fn spawn_stdin_test_child(script: &str) -> tokio::process::Child {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("test child should spawn")
    }

    struct ErroringWriter;

    impl AsyncWrite for ErroringWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "synthetic write failure")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn build_ffprobe_proxy_url_injects_credentials() {
        let proxy_cfg = ProxyConfig {
            url: "http://proxy.local:8080".to_string(),
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
        };
        let resolved = build_ffprobe_proxy_url(&proxy_cfg).expect("proxy url should parse");
        assert!(resolved.contains("alice:secret@proxy.local:8080"));
    }

    #[test]
    fn build_ffprobe_proxy_url_keeps_existing_inline_credentials() {
        let proxy_cfg = ProxyConfig {
            url: "socks5://bob:pass@proxy.local:1080".to_string(),
            username: None,
            password: None,
        };
        let resolved = build_ffprobe_proxy_url(&proxy_cfg).expect("proxy url should parse");
        assert!(resolved.contains("bob:pass@proxy.local:1080"));
    }

    #[test]
    fn build_thumbnail_scale_filter_formats_dimensions() {
        let filter = build_thumbnail_scale_filter(320, 180);
        assert_eq!(filter, "scale=320:180:force_original_aspect_ratio=increase,crop=320:180");
    }

    #[test]
    fn build_thumbnail_scale_filter_uses_defaults_for_zero_dimensions() {
        let filter = build_thumbnail_scale_filter(0, 0);
        let expected = build_thumbnail_scale_filter(default_thumbnail_width(), default_thumbnail_height());
        assert_eq!(filter, expected);
    }

    #[test]
    fn build_thumbnail_args_encodes_expected_ffmpeg_call() {
        let args = build_thumbnail_args("/tmp/in.mkv", Path::new("/tmp/thumb.jpg"), "scale=320:180", 180);
        assert_eq!(
            args,
            vec![
                "-ss",
                "180",
                "-i",
                "/tmp/in.mkv",
                "-frames:v",
                "1",
                "-vf",
                "scale=320:180",
                "-q:v",
                "1",
                "-y",
                "/tmp/thumb.jpg",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn format_ffmpeg_timeout_error_includes_binary_timeout_and_args() {
        let msg = format_ffmpeg_timeout_error(&["-ss".to_string(), "180".to_string(), "-i".to_string(), "/tmp/in.mkv".to_string()]);
        assert!(msg.contains("ffmpeg"));
        assert!(msg.contains(&FFMPEG_TIMEOUT.as_secs().to_string()));
        assert!(msg.contains("-ss 180 -i /tmp/in.mkv"));
    }

    #[test]
    fn is_not_found_probe_error_only_matches_http_404_markers() {
        assert!(super::is_not_found_probe_error("HTTP error 404 Not Found"));
        assert!(super::is_not_found_probe_error("Server returned 404 Not Found"));
        assert!(super::is_not_found_probe_error("HTTP/1.1 404 Not Found"));
        assert!(!super::is_not_found_probe_error("host not found"));
        assert!(!super::is_not_found_probe_error("file not found"));
        assert!(!super::is_not_found_probe_error("protocol handler not found"));
    }

    #[test]
    fn probe_support_rejects_hls_manifest_urls() {
        assert!(!super::is_supported_probe_url("http://provider.example/live/master.m3u8"));
        assert!(!super::is_supported_probe_url("http://provider.example/live/master.m3u8?token=abc"));
        assert!(super::is_supported_probe_url("http://provider.example/live/stream.ts"));
        assert!(super::is_supported_probe_url("file:///var/media/movie.mkv"));
    }

    #[test]
    fn extract_probe_stream_stats_prefers_format_section() {
        let payload = json!({
            "format": {
                "duration": "1541.4",
                "bit_rate": "3100000"
            },
            "streams": []
        });

        assert_eq!(
            extract_probe_stream_stats(&payload, None, None),
            ProbeStreamStats {
                duration_secs: Some(1541),
                bitrate: Some(3_100_000),
            }
        );
    }

    #[test]
    fn extract_probe_stream_stats_falls_back_to_stream_values() {
        let payload = json!({});
        let video = json!({
            "duration": "120.0",
            "bit_rate": "1500"
        });

        assert_eq!(
            extract_probe_stream_stats(&payload, Some(&video), None),
            ProbeStreamStats {
                duration_secs: Some(120),
                bitrate: Some(1500),
            }
        );
    }

    #[test]
    fn seekable_probe_window_bytes_respects_probe_size_cap() {
        assert_eq!(seekable_probe_window_bytes(0), 32 * 1024 * 1024);
        assert_eq!(seekable_probe_window_bytes(1), 1);
        assert_eq!(seekable_probe_window_bytes(64 * 1024 * 1024), 32 * 1024 * 1024);
    }

    #[tokio::test]
    async fn ensure_probe_file_length_preserves_remote_logical_length() {
        let tempdir = tempfile::tempdir().expect("tempdir should succeed");
        let temp_path = tempdir.path().join("probe.bin");
        let remote_logical_length = 10 * 1024 * 1024;

        ensure_probe_file_length(&temp_path, Some(remote_logical_length))
            .await
            .expect("file length should be set");

        let metadata = tokio::fs::metadata(&temp_path)
            .await
            .expect("probe file metadata should be readable");
        assert_eq!(metadata.len(), remote_logical_length);
    }

    #[test]
    fn parse_content_range_total_length_extracts_total() {
        assert_eq!(
            parse_content_range_total_length("bytes 10-20/999"),
            Some(999)
        );
        assert_eq!(parse_content_range_total_length("bytes 0-1/*"), None);
    }

    #[tokio::test]
    async fn fetch_probe_bytes_caps_response_and_sets_headers() {
        let (url, request_capture, accepted, handle) = match start_test_http_server("HTTP/1.1 200 OK", b"abcdefghij").await
        {
            Ok(server) => server,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping fetch_probe_bytes_caps_response_and_sets_headers: {err}");
                return;
            }
            Err(err) => panic!("failed to start test http server: {err}"),
        };

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("http client should build");
        let bytes = fetch_probe_bytes(&client, &url, Some("test-agent/1.0"), 5)
            .await
            .expect("fetch should succeed");

        assert_eq!(bytes, b"abcde");
        assert_eq!(accepted.load(Ordering::SeqCst), 1);

        let request_text =
            String::from_utf8_lossy(&request_capture.lock().expect("request capture mutex poisoned")).to_ascii_lowercase();
        assert!(request_text.contains("range: bytes=0-4"));
        assert!(request_text.contains("user-agent: test-agent/1.0"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_probe_bytes_maps_404_to_not_found() {
        let (url, _request_capture, _accepted, handle) =
            match start_test_http_server("HTTP/1.1 404 Not Found", b"missing").await {
                Ok(server) => server,
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                    eprintln!("skipping fetch_probe_bytes_maps_404_to_not_found: {err}");
                    return;
                }
                Err(err) => panic!("failed to start test http server: {err}"),
            };

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("http client should build");
        let result = fetch_probe_bytes(&client, &url, None, 16).await;

        assert!(matches!(result, Err(ProbeFailureKind::NotFound)));

        handle.abort();
    }

    #[tokio::test]
    async fn stage_seekable_probe_file_writes_head_and_tail_segments() {
        let body = Arc::new(b"abcdefghijklmnopqrstuvwxyz".to_vec());
        let (url, accepted, handle) = start_range_test_http_server(Arc::clone(&body))
            .await
            .expect("range server should start");
        let tempdir = tempfile::tempdir().expect("tempdir should succeed");
        let temp_path = tempdir.path().join("probe.bin");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("http client should build");

        let stage = stage_seekable_probe_file(&client, &url, Some("test-agent/1.0"), 5, &temp_path)
            .await
            .expect("seekable staging should succeed");

        assert_eq!(stage.content_length, Some(26));
        assert_eq!(stage.head_bytes, 5);
        assert_eq!(stage.tail_start, Some(21));
        assert!(!stage.fully_materialized);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);

        let staged = tokio::fs::read(&temp_path).await.expect("staged file should be readable");
        assert_eq!(staged.len(), 26);
        assert_eq!(&staged[..5], b"abcde");
        assert_eq!(&staged[5..21], &[0; 16]);
        assert_eq!(&staged[21..], b"vwxyz");

        handle.abort();
    }

    #[tokio::test]
    async fn stage_seekable_probe_file_with_zero_probe_size_uses_bounded_default_window() {
        let body = Arc::new(b"abcdefghijklmnopqrstuvwxyz".to_vec());
        let (url, accepted, handle) = start_range_test_http_server(Arc::clone(&body))
            .await
            .expect("range server should start");
        let tempdir = tempfile::tempdir().expect("tempdir should succeed");
        let temp_path = tempdir.path().join("probe.bin");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("http client should build");

        let stage = stage_seekable_probe_file(&client, &url, Some("test-agent/1.0"), 0, &temp_path)
            .await
            .expect("seekable staging should succeed");

        assert_eq!(stage.content_length, Some(26));
        assert_eq!(stage.head_bytes, 26);
        assert_eq!(stage.tail_start, None);
        assert!(stage.fully_materialized);
        assert_eq!(accepted.load(Ordering::SeqCst), 1);

        let staged = tokio::fs::read(&temp_path).await.expect("staged file should be readable");
        assert_eq!(staged, *body);

        handle.abort();
    }

    #[tokio::test]
    async fn run_ffprobe_with_stdin_tolerates_early_child_exit_on_success() {
        let child = spawn_stdin_test_child(
            "dd bs=1 count=1 of=/dev/null 2>/dev/null; \
             printf '%s' '{\"streams\":[{\"codec_type\":\"video\",\"codec_name\":\"h264\",\"width\":1920,\"height\":1080},{\"codec_type\":\"audio\",\"codec_name\":\"aac\",\"channels\":2}],\"format\":{\"duration\":\"10\",\"bit_rate\":\"1000\"}}'",
        );
        let probe_bytes = vec![b'x'; 1024 * 1024];

        let outcome = run_ffprobe_with_stdin(child, &probe_bytes, "https://example.com/live.ts").await;

        assert!(matches!(outcome, ProbeUrlOutcome::Success(..)));
    }

    #[tokio::test]
    async fn run_ffprobe_with_stdin_uses_child_failure_after_early_close() {
        let child = spawn_stdin_test_child(
            "dd bs=1 count=1 of=/dev/null 2>/dev/null; \
             printf '%s' 'Invalid data found when processing input' >&2; \
             exit 1",
        );
        let probe_bytes = vec![b'x'; 1024 * 1024];

        let outcome = run_ffprobe_with_stdin(child, &probe_bytes, "https://example.com/live.ts").await;

        assert!(matches!(outcome, ProbeUrlOutcome::Failed(ProbeFailureKind::Other)));
    }

    #[tokio::test]
    async fn write_probe_bytes_to_stdin_keeps_non_broken_pipe_failures_fatal() {
        let mut writer = ErroringWriter;
        let result = write_probe_bytes_to_stdin(&mut writer, b"abcdef", "https://example.com/live.ts").await;

        assert!(matches!(result, Err(ProbeFailureKind::Other)));
    }
}
