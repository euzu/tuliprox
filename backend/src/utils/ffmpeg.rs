use log::{debug, warn};
use tokio::process::Command;
use std::time::Duration;
use crate::model::{MediaQuality};

// Checks if ffprobe is available in the system path
pub async fn check_ffprobe_availability() -> bool {
    match Command::new("ffprobe").arg("-version").output().await {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

pub async fn probe_url(url: &str, user_agent: Option<&str>, analyze_duration: u64, probe_size: u64, timeout_secs: u64) -> Option<MediaQuality> {
    // Determine timeout: Ensure it's at least as long as the analyze duration + buffer, 
    // but respect the user setting if it's longer.
    let analyze_overhead = Duration::from_micros(analyze_duration) + Duration::from_secs(5);
    let config_timeout = Duration::from_secs(timeout_secs);
    let timeout_val = std::cmp::max(analyze_overhead, config_timeout);

    let mut command = Command::new("ffprobe");
    
    command
        .arg("-v").arg("error")
        // Select video stream 0
        .arg("-select_streams").arg("v:0")
        .arg("-show_entries").arg("stream=width,height,codec_name,pix_fmt,color_transfer,codec_tag_string")
        // Select audio stream 0
        .arg("-select_streams").arg("a:0")
        .arg("-show_entries").arg("stream=codec_name,channels,channel_layout")
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
                debug!("ffprobe failed for {}: {}", url, String::from_utf8_lossy(&output.stderr));
                return None;
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
                 let streams = json.get("streams").and_then(|s| s.as_array());

                 if let Some(stream_list) = streams {
                     let mut video_info: Option<String> = None;
                     let mut audio_info: Option<String> = None;

                     for stream in stream_list {
                         // Heuristic to detect video vs audio since we requested specific streams but order/presence isn't guaranteed
                         let is_video = stream.get("width").is_some() || stream.get("height").is_some();
                         let is_audio = stream.get("channels").is_some() || stream.get("channel_layout").is_some();

                         if is_video && video_info.is_none() {
                             video_info = Some(stream.to_string());
                         } else if is_audio && audio_info.is_none() {
                             audio_info = Some(stream.to_string());
                         }
                     }

                     if video_info.is_some() || audio_info.is_some() {
                         return MediaQuality::from_ffprobe_info(audio_info.as_deref(), video_info.as_deref());
                     }
                 }
            } else {
                warn!("Failed to parse ffprobe json output for {}", url);
            }
        }
        Ok(Err(e)) => {
            warn!("ffprobe execution failed for {}: {}", url, e);
        }
        Err(_) => {
            warn!("ffprobe timed out after {:?} for {}", timeout_val, url);
        }
    }

    None
}