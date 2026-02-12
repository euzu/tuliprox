use log::{debug, warn};
use tokio::process::Command;
use std::time::Duration;
use serde_json::Value;
use shared::model::MediaQuality;
use shared::utils::sanitize_sensitive_info;

// Checks if ffprobe is available in the system path
pub async fn check_ffprobe_availability() -> bool {
    match Command::new("ffprobe").arg("-version").output().await {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

pub async fn probe_url(url: &str, user_agent: Option<&str>, analyze_duration: u64, probe_size: u64, timeout_secs: u64) -> Option<(MediaQuality, Option<Value>, Option<Value>)> {
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
                return None;
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(json) = serde_json::from_str::<Value>(&stdout) {
                 let streams = json.get("streams").and_then(|s| s.as_array());

                 if let Some(stream_list) = streams {
                     let mut video_info: Option<String> = None;
                     let mut audio_info: Option<String> = None;
                     let mut raw_video_json: Option<Value> = None;
                     let mut raw_audio_json: Option<Value> = None;

                     for stream in stream_list {
                         // Check codec_type if available
                         let codec_type = stream.get("codec_type").and_then(|s| s.as_str());
                         
                         // We prefer the first video/audio stream we find
                         if codec_type == Some("video") && video_info.is_none() {
                             video_info = Some(stream.to_string());
                             raw_video_json = Some(stream.clone());
                         } else if codec_type == Some("audio") && audio_info.is_none() {
                             audio_info = Some(stream.to_string());
                             raw_audio_json = Some(stream.clone());
                         }
                     }

                     // Fallback heuristic if codec_type missing
                     if video_info.is_none() {
                         for stream in stream_list {
                             if (stream.get("width").is_some() || stream.get("height").is_some()) && video_info.is_none() {
                                 video_info = Some(stream.to_string());
                                 raw_video_json = Some(stream.clone());
                             }
                         }
                     }
                     if audio_info.is_none() {
                         for stream in stream_list {
                              if (stream.get("channels").is_some() || stream.get("channel_layout").is_some()) && audio_info.is_none() {
                                 audio_info = Some(stream.to_string());
                                 raw_audio_json = Some(stream.clone());
                             }
                         }
                     }

                     if video_info.is_some() || audio_info.is_some() {
                         let mq = MediaQuality::from_ffprobe_info(audio_info.as_deref(), video_info.as_deref());
                         if let Some(quality) = mq {
                             return Some((quality, raw_video_json, raw_audio_json));
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

    None
}