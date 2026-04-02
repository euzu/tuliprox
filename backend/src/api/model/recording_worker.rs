use crate::api::model::{DownloadControl, FileDownload};
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingExecutionResult {
    Completed,
    Paused,
    Cancelled,
    Preempted,
    Retryable(String),
    Failed(String),
}

fn stderr_summary(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map_or_else(|| "ffmpeg failed".to_string(), ToString::to_string)
}

fn is_retryable_ffmpeg_failure_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("connection timed out")
        || msg.contains("timed out")
        || msg.contains("temporarily unavailable")
        || msg.contains("temporary failure")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("network is unreachable")
        || msg.contains("server returned 5")
        || msg.contains("http error 5")
        || msg.contains("503 service unavailable")
        || msg.contains("502 bad gateway")
        || msg.contains("tls")
        || msg.contains("i/o error")
}

fn classify_ffmpeg_failure(stderr: &[u8]) -> RecordingExecutionResult {
    let summary = stderr_summary(stderr);
    if is_retryable_ffmpeg_failure_message(&summary) {
        RecordingExecutionResult::Retryable(summary)
    } else {
        RecordingExecutionResult::Failed(summary)
    }
}

pub fn remaining_recording_duration_secs(download: &FileDownload, now_ts: i64) -> Option<u64> {
    match (download.start_at, download.duration_secs) {
        (_, None) => None,
        (None, Some(duration_secs)) => Some(duration_secs),
        (Some(start_at), Some(duration_secs)) => {
            let duration_i64 = i64::try_from(duration_secs).unwrap_or(i64::MAX);
            let end_at = start_at.saturating_add(duration_i64);
            if now_ts >= end_at {
                None
            } else if now_ts <= start_at {
                Some(duration_secs)
            } else {
                u64::try_from(end_at.saturating_sub(now_ts)).ok()
            }
        }
    }
}

pub fn build_recording_args(download: &FileDownload, effective_duration_secs: u64) -> Vec<String> {
    vec![
        "-y".to_string(),
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-i".to_string(),
        download.url.to_string(),
        "-map".to_string(),
        "0".to_string(),
        "-t".to_string(),
        effective_duration_secs.to_string(),
        "-c".to_string(),
        "copy".to_string(),
        download.file_path.to_string_lossy().to_string(),
    ]
}

pub fn recording_start_missed_window(download: &FileDownload, now_ts: i64) -> bool {
    download
        .start_at
        .zip(download.duration_secs)
        .is_some_and(|(start_at, duration_secs)| now_ts >= start_at.saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX)))
}

pub async fn run_recording(
    download: &FileDownload,
    control_signal: &RwLock<DownloadControl>,
    control_notify: &Notify,
    cancel_token: Option<&CancellationToken>,
) -> RecordingExecutionResult {
    let now_ts = chrono::Utc::now().timestamp();
    if recording_start_missed_window(download, now_ts) {
        return RecordingExecutionResult::Failed("Recording window already expired".to_string());
    }
    let Some(effective_duration_secs) = remaining_recording_duration_secs(download, now_ts) else {
        return RecordingExecutionResult::Failed("Recording window already expired".to_string());
    };

    if let Err(err) = tokio::fs::create_dir_all(&download.file_dir).await {
        return RecordingExecutionResult::Failed(format!("Error while creating recording directory: {err}"));
    }

    let mut command = tokio::process::Command::new("ffmpeg");
    command
        .args(build_recording_args(download, effective_duration_secs))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return RecordingExecutionResult::Failed(format!("Failed to spawn ffmpeg: {err}")),
    };

    let mut wait_future = Box::pin(child.wait_with_output());

    loop {
        tokio::select! {
            biased;
            () = async {
                if let Some(token) = cancel_token {
                    token.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => return RecordingExecutionResult::Preempted,
            () = control_notify.notified() => {
                match *control_signal.read().await {
                    DownloadControl::Pause => return RecordingExecutionResult::Paused,
                    DownloadControl::Cancel => return RecordingExecutionResult::Cancelled,
                    DownloadControl::None => {}
                }
            }
            output = &mut wait_future => {
                match output {
                    Ok(output) if output.status.success() => return RecordingExecutionResult::Completed,
                    Ok(output) => return classify_ffmpeg_failure(&output.stderr),
                    Err(err) => return RecordingExecutionResult::Failed(format!("Failed to wait for ffmpeg: {err}")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RecordingExecutionResult, build_recording_args, classify_ffmpeg_failure, recording_start_missed_window,
        remaining_recording_duration_secs,
    };
    use crate::api::model::{DownloadKind, DownloadState, FileDownload};
    use std::path::PathBuf;

    fn make_recording(start_at: i64, duration_secs: u64) -> FileDownload {
        FileDownload {
            uuid: "id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/recording.ts"),
            filename: "recording.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/live/1").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Scheduled,
            start_at: Some(start_at),
            duration_secs: Some(duration_secs),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
        }
    }

    #[test]
    fn build_recording_args_maps_duration_and_output_path() {
        let recording = make_recording(1_000, 5400);
        let args = build_recording_args(&recording, 5400);

        assert!(args.windows(2).any(|pair| pair == ["-t", "5400"]));
        assert!(args.windows(2).any(|pair| pair == ["-i", "https://example.com/live/1"]));
        assert_eq!(args.last().map(String::as_str), Some("/tmp/recording.ts"));
    }

    #[test]
    fn recording_start_missed_window_rejects_overdue_recording() {
        let recording = make_recording(1_000, 60);
        assert!(!recording_start_missed_window(&recording, 1_059));
        assert!(recording_start_missed_window(&recording, 1_060));
    }

    #[test]
    fn remaining_recording_duration_tracks_remaining_window() {
        let recording = make_recording(1_000, 60);
        assert_eq!(remaining_recording_duration_secs(&recording, 900), Some(60));
        assert_eq!(remaining_recording_duration_secs(&recording, 1_000), Some(60));
        assert_eq!(remaining_recording_duration_secs(&recording, 1_030), Some(30));
        assert_eq!(remaining_recording_duration_secs(&recording, 1_059), Some(1));
        assert_eq!(remaining_recording_duration_secs(&recording, 1_060), None);
    }

    #[test]
    fn classify_ffmpeg_failure_marks_transient_transport_errors_retryable() {
        let result = classify_ffmpeg_failure(b"Last message\nConnection timed out\n");
        assert_eq!(
            result,
            RecordingExecutionResult::Retryable("Connection timed out".to_string())
        );
    }

    #[test]
    fn classify_ffmpeg_failure_keeps_terminal_usage_errors_failed() {
        let result = classify_ffmpeg_failure(b"Last message\nInvalid argument\n");
        assert_eq!(result, RecordingExecutionResult::Failed("Invalid argument".to_string()));
    }
}
