use crate::model::ProxyConfig;
use log::{debug, warn};
use serde_json::Value;
use shared::model::MediaQuality;
use shared::utils::{default_thumbnail_height, default_thumbnail_width, sanitize_sensitive_info};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use url::Url;

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailureKind {
    NotFound,
    Cancelled,
    Other,
}

pub enum ProbeUrlOutcome {
    Success(MediaQuality, Option<Value>, Option<Value>),
    Failed(ProbeFailureKind),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FfmpegExecutor;

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
            Ok(Ok(output)) => {
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
                                    || (codec_type.is_none()
                                        && (stream.get("width").is_some() || stream.get("height").is_some())))
                                && !is_attached_pic(stream)
                            {
                                video_stream = Some(stream);
                            } else if audio_stream.is_none()
                                && (codec_type == Some("audio")
                                    || (codec_type.is_none()
                                        && (stream.get("channels").is_some()
                                            || stream.get("channel_layout").is_some())))
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
                                return ProbeUrlOutcome::Success(
                                    quality,
                                    video_stream.cloned(),
                                    audio_stream.cloned(),
                                );
                            }
                        }
                    }
                } else {
                    warn!("Failed to parse ffprobe json output for {}", sanitize_sensitive_info(url));
                }
            }
            Ok(Err(e)) => {
                warn!("ffprobe execution failed for {}: {}", sanitize_sensitive_info(url), e);
            }
            Err(_) => {
                warn!("ffprobe timed out after {:?} for {}", timeout_val, sanitize_sensitive_info(url));
            }
        }

        ProbeUrlOutcome::Failed(ProbeFailureKind::Other)
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
}

fn format_ffmpeg_timeout_error(args: &[String]) -> String {
    let summary = args.join(" ");
    format!(
        "Timed out running ffmpeg after {}s: {summary}",
        FFMPEG_TIMEOUT.as_secs()
    )
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
    normalized.contains("404") || normalized.contains("not found")
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
    use super::{build_ffprobe_proxy_url, build_thumbnail_args, build_thumbnail_scale_filter, format_ffmpeg_timeout_error, FFMPEG_TIMEOUT};
    use crate::model::ProxyConfig;
    use std::path::Path;

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
}
