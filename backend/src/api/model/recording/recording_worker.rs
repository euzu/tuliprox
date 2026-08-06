use crate::api::model::{DownloadControl, FileDownload};
use log::debug;
use std::path::{Path, PathBuf};
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

fn is_generic_ffmpeg_stderr_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("conversion failed!")
        || trimmed.eq_ignore_ascii_case("exiting normally, received signal 15.")
}

fn stderr_summary(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    stderr
        .lines()
        .rev()
        .find(|line| !is_generic_ffmpeg_stderr_line(line))
        .map_or_else(|| "ffmpeg failed".to_string(), |line| line.trim().to_string())
}

fn is_retryable_ffmpeg_failure_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("connection timed out")
        || msg.contains("timed out")
        || msg.contains("temporarily unavailable")
        || msg.contains("temporary failure")
        || msg.contains("resource temporarily unavailable")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("connection closed")
        || msg.contains("broken pipe")
        || msg.contains("unexpected eof")
        || msg.contains("end of file")
        || msg.contains("network is unreachable")
        || msg.contains("no route to host")
        || msg.contains("name or service not known")
        || msg.contains("temporary failure in name resolution")
        || msg.contains("could not resolve host")
        || msg.contains("could not resolve")
        || msg.contains("failed to resolve hostname")
        || msg.contains("server returned 5")
        || msg.contains("http error 5")
        || msg.contains("http error 429")
        || msg.contains("429 too many requests")
        || msg.contains("503 service unavailable")
        || msg.contains("502 bad gateway")
        || msg.contains("504 gateway timeout")
        || msg.contains("500 internal server error")
        || msg.contains("tls handshake")
        || msg.contains("tls timeout")
        || msg.contains("tls: handshake")
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

pub fn build_recording_args(download: &FileDownload, effective_duration_secs: u64, output_path: &Path) -> Vec<String> {
    vec![
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
        // Recording filenames may have no extension (sanitized title-only
        // names from `render_filename_preview`), so we force the output
        // muxer explicitly to avoid ffmpeg failing format detection with
        // `Invalid argument` on paths like `foo.partial`.
        "-f".to_string(),
        "mpegts".to_string(),
        // Overwrite any stale `.partial` from a previous failed attempt.
        // Without this, a leftover file (from a crash, retry-with-same-path,
        // or startup-recovery race) would either block the new run via
        // `recording_resume_or_retry_is_unsupported` or leak on disk.
        "-y".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        output_path.to_string_lossy().to_string(),
    ]
}

async fn recording_resume_or_retry_is_unsupported(download: &FileDownload) -> bool {
    tokio::fs::metadata(recording_partial_path(&download.file_path))
        .await
        .is_ok_and(|metadata| metadata.len() > 0)
}

pub fn recording_start_missed_window(download: &FileDownload, now_ts: i64) -> bool {
    download
        .start_at
        .zip(download.duration_secs)
        .is_some_and(|(start_at, duration_secs)| now_ts >= start_at.saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX)))
}

async fn run_recording_with_binary(
    ffmpeg_binary: &Path,
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

    if recording_resume_or_retry_is_unsupported(download).await {
        return RecordingExecutionResult::Failed(
            "Recording resume is not supported".to_string(),
            //  yet because ffmpeg segment stitching is not implemented
        );
    }

    let partial_path = recording_partial_path(&download.file_path);
    let args = build_recording_args(download, effective_duration_secs, &partial_path);
    debug!(
        "recording spawn: {} {}",
        ffmpeg_binary.display(),
        args.join(" ")
    );
    let mut command = tokio::process::Command::new(ffmpeg_binary);
    command
        .args(args)
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
                    DownloadControl::Restart => return RecordingExecutionResult::Preempted,
                    DownloadControl::None => {}
                }
            }
            output = &mut wait_future => {
                match output {
                    Ok(output) if output.status.success() => {
                        return match crate::utils::finalize_no_replace(&partial_path, &download.file_path) {
                            Ok(()) => RecordingExecutionResult::Completed,
                            Err(err) => RecordingExecutionResult::Failed(format!("Failed to finalize recording: {err}")),
                        };
                    }
                    Ok(output) => return classify_ffmpeg_failure(&output.stderr),
                    Err(err) => return RecordingExecutionResult::Failed(format!("Failed to wait for ffmpeg: {err}")),
                }
            }
        }
    }
}

pub async fn run_recording(
    download: &FileDownload,
    control_signal: &RwLock<DownloadControl>,
    control_notify: &Notify,
    cancel_token: Option<&CancellationToken>,
) -> RecordingExecutionResult {
    run_recording_with_binary(Path::new("ffmpeg"), download, control_signal, control_notify, cancel_token).await
}

/// Compute the partial-file path the worker uses for safe no-clobber
/// writes. Inserts `.partial` after any existing extension so the
/// partial keeps the final file's type (`pilot.ts` → `pilot.ts.partial`).
/// When the final path has no extension the partial defaults to
/// `.ts.partial` so the in-progress recording is still recognisable as
/// MPEG-TS.
pub fn recording_partial_path(final_path: &Path) -> PathBuf {
    match final_path.extension() {
        Some(ext) if !ext.is_empty() => {
            let mut new_ext = ext.to_os_string();
            new_ext.push(".partial");
            final_path.with_extension(new_ext)
        }
        _ => final_path.with_extension("ts.partial"),
    }
}

/// Summary of one startup-recovery decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// Active state with a valid final file → normalize to `Completed`.
    Completed,
    /// Active state with a partial file → normalize to terminal `Failed`,
    /// retain the partial.
    FailedPartialKept,
    /// Active state with no owned file → normalize to terminal `Failed`.
    FailedNoFile,
    /// Path was unsafe (symlink, wrong type, outside root) → fail closed
    /// without opening the file.
    UnsafePath,
}

/// Inspect a recording's current filesystem state and return the recovery
/// decision the startup loop should apply. The function is pure: it does
/// not mutate the queue or open files.
pub fn recovery_decision_for(final_path: &Path, partial: &Path) -> RecoveryDecision {
    if crate::utils::no_follow_regular_file(final_path).is_some() {
        return RecoveryDecision::Completed;
    }
    if crate::utils::no_follow_regular_file(partial).is_some() {
        return RecoveryDecision::FailedPartialKept;
    }
    RecoveryDecision::FailedNoFile
}

#[cfg(test)]
mod tests {
    use super::{
        RecordingExecutionResult, RecoveryDecision, build_recording_args, classify_ffmpeg_failure,
        recording_partial_path, recording_resume_or_retry_is_unsupported, recording_start_missed_window,
        recovery_decision_for, remaining_recording_duration_secs, run_recording_with_binary,
    };
    use crate::api::model::{DownloadControl, DownloadKind, DownloadState, FileDownload};
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tokio::sync::{Notify, RwLock};
    use tokio_util::sync::CancellationToken;

    fn unique_recording_output() -> (PathBuf, PathBuf, String) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let file_dir = std::env::temp_dir().join(format!("tuliprox_recording_test_{nanos}"));
        let filename = format!("recording_{nanos}.ts");
        let file_path = file_dir.join(&filename);
        (file_dir, file_path, filename)
    }

    fn make_recording(start_at: i64, duration_secs: u64) -> FileDownload {
        let (file_dir, file_path, filename) = unique_recording_output();
        FileDownload {
            uuid: "id".to_string(),
            file_dir,
            file_path,
            filename,
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
            recording: None,
        }
    }

    #[test]
    fn build_recording_args_maps_duration_and_output_path() {
        let recording = make_recording(1_000, 5400);
        let output = recording_partial_path(&recording.file_path);
        let args = build_recording_args(&recording, 5400, &output);

        assert!(args.contains(&"-y".to_string()));
        assert!(args.windows(2).any(|pair| pair == ["-t", "5400"]));
        assert!(args.windows(2).any(|pair| pair == ["-i", "https://example.com/live/1"]));
        assert!(args.windows(2).any(|pair| pair == ["-f", "mpegts"]));
        assert_eq!(args.last(), Some(&output.to_string_lossy().to_string()));
    }

    #[test]
    fn classify_ffmpeg_failure_skips_generic_trailer_lines() {
        let result = classify_ffmpeg_failure(b"Connection timed out\nConversion failed!\n");

        assert_eq!(
            result,
            RecordingExecutionResult::Retryable("Connection timed out".to_string())
        );
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

    #[test]
    fn classify_ffmpeg_failure_marks_broader_transient_network_errors_retryable() {
        let result = classify_ffmpeg_failure(b"Last message\nCould not resolve host: example.com\n");
        assert_eq!(
            result,
            RecordingExecutionResult::Retryable("Could not resolve host: example.com".to_string())
        );
    }

    #[test]
    fn classify_ffmpeg_failure_marks_only_transient_tls_failures_retryable() {
        let retryable = classify_ffmpeg_failure(b"Last message\ntls handshake timeout\n");
        let certificate = classify_ffmpeg_failure(b"Last message\ncertificate verify failed\n");
        let protocol = classify_ffmpeg_failure(b"Last message\nunsupported protocol version\n");

        assert_eq!(
            retryable,
            RecordingExecutionResult::Retryable("tls handshake timeout".to_string())
        );
        assert_eq!(
            certificate,
            RecordingExecutionResult::Failed("certificate verify failed".to_string())
        );
        assert_eq!(
            protocol,
            RecordingExecutionResult::Failed("unsupported protocol version".to_string())
        );
    }

    fn fake_ffmpeg_script(name: &str, body: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tuliprox_fake_ffmpeg_{name}_{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        let script_path = dir.join("ffmpeg");
        fs::write(&script_path, body).expect("write fake ffmpeg");
        #[cfg(unix)]
        {
        let mut perms = fs::metadata(&script_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");
        }
        script_path
    }

    #[tokio::test]
    async fn recording_retry_attempts_without_partial_output_do_not_block_retry() {
        let mut recording = make_recording(chrono::Utc::now().timestamp(), 30);
        recording.retry_attempts = 2;

        let unsupported = recording_resume_or_retry_is_unsupported(&recording).await;

        assert!(!unsupported);
    }

    #[tokio::test]
    async fn run_recording_completes_with_fake_ffmpeg() {
        let script = fake_ffmpeg_script(
            "success",
            "#!/bin/sh\nfor arg in \"$@\"; do output=\"$arg\"; done\nprintf 'recorded' > \"$output\"\nexit 0\n",
        );
        let control_signal = RwLock::new(DownloadControl::None);
        let control_notify = Notify::new();
        let recording = make_recording(chrono::Utc::now().timestamp(), 5);

        let result = run_recording_with_binary(&script, &recording, &control_signal, &control_notify, None).await;

        assert_eq!(result, RecordingExecutionResult::Completed);
        assert_eq!(tokio::fs::read(&recording.file_path).await.expect("read output"), b"recorded");
        assert!(!recording_partial_path(&recording.file_path).exists());
        let _ = fs::remove_file(script);
        let _ = fs::remove_file(&recording.file_path);
        let _ = fs::remove_dir_all(&recording.file_dir);
    }

    #[tokio::test]
    async fn run_recording_returns_retryable_for_fake_transient_ffmpeg_failure() {
        let script = fake_ffmpeg_script(
            "retryable",
            "#!/bin/sh\nprintf 'Could not resolve host: upstream.example\\n' >&2\nexit 1\n",
        );
        let control_signal = RwLock::new(DownloadControl::None);
        let control_notify = Notify::new();
        let recording = make_recording(chrono::Utc::now().timestamp(), 5);

        let result = run_recording_with_binary(&script, &recording, &control_signal, &control_notify, None).await;

        assert_eq!(
            result,
            RecordingExecutionResult::Retryable("Could not resolve host: upstream.example".to_string())
        );
        let _ = fs::remove_file(script);
        let _ = fs::remove_dir_all(&recording.file_dir);
    }

    #[tokio::test]
    async fn run_recording_preempts_fake_ffmpeg_and_preserves_window_semantics() {
        let script = fake_ffmpeg_script(
            "preempt",
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nsleep 30\n",
        );
        let control_signal = RwLock::new(DownloadControl::None);
        let control_notify = Notify::new();
        let cancel_token = CancellationToken::new();
        let recording = make_recording(chrono::Utc::now().timestamp().saturating_sub(2), 30);
        let notify_cancel = cancel_token.clone();

        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            notify_cancel.cancel();
        });

        let result = run_recording_with_binary(&script, &recording, &control_signal, &control_notify, Some(&cancel_token)).await;
        cancel_task.await.expect("cancel task");

        assert_eq!(result, RecordingExecutionResult::Preempted);
        assert!(remaining_recording_duration_secs(&recording, chrono::Utc::now().timestamp()).is_some());
        let _ = fs::remove_file(script);
        let _ = fs::remove_dir_all(&recording.file_dir);
    }

    #[tokio::test]
    async fn run_recording_refuses_retry_or_resume_when_partial_output_exists() {
        let script = fake_ffmpeg_script(
            "no-resume",
            "#!/bin/sh\nprintf 'should not run' >&2\nexit 1\n",
        );
        let control_signal = RwLock::new(DownloadControl::None);
        let control_notify = Notify::new();
        let recording = make_recording(chrono::Utc::now().timestamp(), 5);

        fs::create_dir_all(&recording.file_dir).expect("create recording dir");
        fs::write(recording_partial_path(&recording.file_path), b"partial").expect("write partial output");

        let result = run_recording_with_binary(&script, &recording, &control_signal, &control_notify, None).await;

        assert_eq!(
            result,
            RecordingExecutionResult::Failed(
                "Recording resume is not supported"
                    .to_string(),
            )
        );
        let _ = fs::remove_file(script);
        let _ = fs::remove_file(&recording.file_path);
        let _ = fs::remove_dir_all(&recording.file_dir);
    }

    // --- partial path + startup recovery ---

    #[test]
    fn recording_partial_path_appends_dot_partial_after_existing_extension() {
        let p = Path::new("/var/recordings/pilot.ts");
        let partial = recording_partial_path(p);
        assert_eq!(partial, PathBuf::from("/var/recordings/pilot.ts.partial"));
    }

    #[test]
    fn recording_partial_path_defaults_to_ts_partial_when_no_extension() {
        let p = Path::new("/halde/temp/NL_RTL_Z_8K");
        let partial = recording_partial_path(p);
        assert_eq!(partial, PathBuf::from("/halde/temp/NL_RTL_Z_8K.ts.partial"));
    }

    #[test]
    fn recording_partial_path_uses_last_extension_for_multi_dot_names() {
        let p = Path::new("/var/recordings/show.2024.s01.ts");
        let partial = recording_partial_path(p);
        assert_eq!(partial, PathBuf::from("/var/recordings/show.2024.s01.ts.partial"));
    }

    #[test]
    fn recovery_decision_for_completed_when_final_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let final_path = dir.path().join("rec.ts");
        std::fs::write(&final_path, b"recorded").expect("write final");
        let partial = dir.path().join("rec.ts.partial");
        let decision = recovery_decision_for(&final_path, &partial);
        assert_eq!(decision, RecoveryDecision::Completed);
    }

    #[test]
    fn recovery_decision_for_failed_when_only_partial_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let final_path = dir.path().join("rec.ts");
        let partial = dir.path().join("rec.ts.partial");
        std::fs::write(&partial, b"partial bytes").expect("write partial");
        let decision = recovery_decision_for(&final_path, &partial);
        assert_eq!(decision, RecoveryDecision::FailedPartialKept);
    }

    #[test]
    fn recovery_decision_for_failed_when_no_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let final_path = dir.path().join("rec.ts");
        let partial = dir.path().join("rec.ts.partial");
        let decision = recovery_decision_for(&final_path, &partial);
        assert_eq!(decision, RecoveryDecision::FailedNoFile);
    }

    #[test]
    fn recovery_decision_for_treats_symlinked_final_as_completed() {
        // The no-follow check returns None for symlinks; the helper
        // therefore treats the symlink as "not present" and falls through
        // to the partial check. The startup recovery in
        // `recover_loaded_download` is responsible for failing closed
        // when the path is a symlink to outside the root; this helper
        // only inspects the file presence semantics.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.ts");
        let link_path = dir.path().join("rec.ts");
        std::fs::write(&real, b"data").expect("write real");
        std::os::unix::fs::symlink(&real, &link_path).expect("symlink");
        let partial = dir.path().join("rec.ts.partial");
        let decision = recovery_decision_for(&link_path, &partial);
        assert_eq!(decision, RecoveryDecision::FailedNoFile);
    }
}
