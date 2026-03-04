use crate::model::ProxyConfig;
use log::{debug, warn};
use serde_json::Value;
use shared::model::MediaQuality;
use shared::utils::sanitize_sensitive_info;
use std::time::Duration;
use tokio::process::Command;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailureKind {
    NotFound,
    Other,
}

pub enum ProbeUrlOutcome {
    Success(MediaQuality, Option<Value>, Option<Value>),
    Failed(ProbeFailureKind),
}

// Checks if ffprobe is available in the system path
pub async fn check_ffprobe_availability() -> bool {
    match Command::new("ffprobe").arg("-version").output().await {
        Ok(output) => output.status.success(),
        Err(_) => false,
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

fn is_not_found_probe_error(stderr: &str) -> bool {
    let normalized = stderr.to_ascii_lowercase();
    normalized.contains("404") || normalized.contains("not found")
}

pub async fn probe_url(
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
        .arg("-show_streams") // Get all streams info
        .arg("-of").arg("json")
        // Optimization for network streams
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
                    // Single-pass stream detection: prefer codec_type, fall back to structural hints.
                    let mut video_stream: Option<&Value> = None;
                    let mut audio_stream: Option<&Value> = None;

                    for stream in stream_list {
                        let codec_type = stream.get("codec_type").and_then(Value::as_str);
                        if video_stream.is_none()
                            && (codec_type == Some("video")
                                || (codec_type.is_none()
                                    && (stream.get("width").is_some() || stream.get("height").is_some())))
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
                        // Materialize strings only for the selected streams.
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

/// Wrapper around [`probe_url`] that races the probe against an optional cancellation token.
/// When the token fires, the probe future is dropped (`kill_on_drop` kills the ffprobe process)
/// and `ProbeUrlOutcome::Failed(Other)` is returned immediately.
pub async fn probe_url_with_cancel(
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
                ProbeUrlOutcome::Failed(ProbeFailureKind::Other)
            }
            result = probe_url(url, user_agent, analyze_duration, probe_size, timeout_secs, proxy_cfg) => result,
        }
    } else {
        probe_url(url, user_agent, analyze_duration, probe_size, timeout_secs, proxy_cfg).await
    }
}

#[cfg(test)]
mod tests {
    use super::build_ffprobe_proxy_url;
    use crate::model::ProxyConfig;

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
}
