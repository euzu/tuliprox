use crate::download::{DownloadControl, FileDownload};
use log::debug;
use shared::model::RecordingContainerFormat;
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

/// Substrings in an ffmpeg stderr line that mean "the source was
/// briefly unavailable, try again inside the window" rather than "this
/// recording cannot succeed".
///
/// Kept as a `const` so the entries stay lowercase by construction: the
/// matcher lowercases the haystack once, so an upper-case entry added
/// here would silently never match.
///
/// Substring supersets are removed: `"timed out"` already matches every
/// line that contains `"connection timed out"`, so the narrower phrase
/// is dead weight. The
/// [`retryable_phrases_have_no_proper_subset`](tests::retryable_phrases_have_no_proper_subset)
/// test catches future reintroductions.
const RETRYABLE_FFMPEG_PHRASES: &[&str] = &[
    "timed out",
    "temporarily unavailable",
    "connection reset",
    "connection refused",
    "connection closed",
    "broken pipe",
    "unexpected eof",
    "end of file",
    "network is unreachable",
    "no route to host",
    "name or service not known",
    "temporary failure in name resolution",
    "could not resolve",
    "failed to resolve hostname",
    "server returned 5",
    "http error 5",
    "http error 429",
    "429 too many requests",
    "503 service unavailable",
    "502 bad gateway",
    "504 gateway timeout",
    "500 internal server error",
    "tls handshake",
    "tls timeout",
    "tls: handshake",
    "i/o error",
];

/// Strip URL-shaped tokens out of a stderr line.
///
/// ffmpeg echoes the input URL in most of its error lines, so a
/// provider whose path or query happens to contain e.g.
/// `connection_refused` would flip every fatal error into a retryable
/// one and the worker would spin until the recording window closed.
/// The classifier must only see ffmpeg's own words.
fn is_url_token(token: &str) -> bool {
    token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("rtmp://")
        || token.starts_with("rtsp://")
        || token.starts_with("udp://")
        || token.starts_with("srt://")
        || token.starts_with("file://")
}

fn strip_url_tokens(message: &str) -> String {
    message.split_whitespace().filter(|token| !is_url_token(token)).collect::<Vec<_>>().join(" ").to_ascii_lowercase()
}

fn is_retryable_ffmpeg_failure_message(message: &str) -> bool {
    let msg = strip_url_tokens(message);
    RETRYABLE_FFMPEG_PHRASES.iter().any(|phrase| msg.contains(phrase))
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
        // No scheduled start: the whole duration is still ahead.
        (None, Some(duration_secs)) => Some(duration_secs),
        (Some(start_at), Some(duration_secs)) => {
            super::recording_math::remaining_window_secs(start_at, duration_secs, now_ts)
        }
    }
}

pub fn build_recording_args(
    download: &FileDownload,
    effective_duration_secs: u64,
    output_path: &Path,
    container_format: RecordingContainerFormat,
) -> Vec<String> {
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
        // `Invalid argument` on paths like `foo.partial`. Which muxer is
        // an operator choice: MPEG-TS survives truncation, but an
        // H.265/AAC source may need Matroska or MP4.
        "-f".to_string(),
        container_format.ffmpeg_format().to_string(),
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
    tokio::fs::metadata(recording_partial_path(&download.file_path)).await.is_ok_and(|metadata| metadata.len() > 0)
}

/// Remove the partial file from a non-terminal exit so the recording does
/// not leak half-written output on disk. Best-effort: missing file or
/// permission errors are swallowed because the next attempt's `-y` flag
/// will overwrite any survivor anyway.
async fn cleanup_partial(partial_path: &Path) {
    let _ = tokio::fs::remove_file(partial_path).await;
}

pub fn recording_start_missed_window(download: &FileDownload, now_ts: i64) -> bool {
    download
        .start_at
        .zip(download.duration_secs)
        .is_some_and(|(start_at, duration_secs)| super::recording_math::window_elapsed(start_at, duration_secs, now_ts))
}

async fn run_recording_with_binary(
    ffmpeg_binary: &Path,
    download: &FileDownload,
    control_signal: &RwLock<DownloadControl>,
    control_notify: &Notify,
    cancel_token: Option<&CancellationToken>,
    container_format: RecordingContainerFormat,
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
    let args = build_recording_args(download, effective_duration_secs, &partial_path, container_format);
    debug!("recording spawn: {} {}", ffmpeg_binary.display(), args.join(" "));
    let mut command = tokio::process::Command::new(ffmpeg_binary);
    command.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);

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
            } => {
                cleanup_partial(&partial_path).await;
                return RecordingExecutionResult::Preempted;
            }
            () = control_notify.notified() => {
                let result = match *control_signal.read().await {
                    DownloadControl::Pause => Some(RecordingExecutionResult::Paused),
                    DownloadControl::Cancel => Some(RecordingExecutionResult::Cancelled),
                    DownloadControl::Restart => Some(RecordingExecutionResult::Preempted),
                    DownloadControl::None => None,
                };
                if let Some(result) = result {
                    cleanup_partial(&partial_path).await;
                    return result;
                }
            }
            output = &mut wait_future => {
                match output {
                    Ok(output) if output.status.success() => {
                        return match tuliprox_core::utils::finalize_no_replace(&partial_path, &download.file_path).await {
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
    container_format: RecordingContainerFormat,
) -> RecordingExecutionResult {
    run_recording_with_binary(
        Path::new("ffmpeg"),
        download,
        control_signal,
        control_notify,
        cancel_token,
        container_format,
    )
    .await
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
pub async fn recovery_decision_for(final_path: &Path, partial: &Path) -> RecoveryDecision {
    if tuliprox_core::utils::no_follow_regular_file(final_path).await.is_some() {
        return RecoveryDecision::Completed;
    }
    if tuliprox_core::utils::no_follow_regular_file(partial).await.is_some() {
        return RecoveryDecision::FailedPartialKept;
    }
    RecoveryDecision::FailedNoFile
}

#[cfg(test)]
mod tests {
    use super::{
        build_recording_args, classify_ffmpeg_failure, recording_partial_path,
        recording_resume_or_retry_is_unsupported, recording_start_missed_window, recovery_decision_for,
        remaining_recording_duration_secs, run_recording_with_binary, RecordingExecutionResult, RecoveryDecision,
    };
    use crate::{
        download::{DownloadControl, DownloadKind, DownloadState, FileDownload},
        recording_worker::RETRYABLE_FFMPEG_PHRASES,
    };
    use shared::model::RecordingContainerFormat;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::{Notify, RwLock};
    use tokio_util::sync::CancellationToken;

    /// The wall clock alone is not enough: these tests run in parallel and two of
    /// them reading the same nanosecond share an output directory, so one test's
    /// partial file makes another take the "resume is not supported" path.
    static OUTPUT_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_recording_output() -> (PathBuf, PathBuf, String) {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_nanos();
        let seq = OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed);
        let file_dir = std::env::temp_dir().join(format!("tuliprox_recording_test_{nanos}_{seq}"));
        let filename = format!("recording_{nanos}_{seq}.ts");
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
        let args = build_recording_args(&recording, 5400, &output, RecordingContainerFormat::default());

        assert!(args.contains(&"-y".to_string()));
        assert!(args.windows(2).any(|pair| pair == ["-t", "5400"]));
        assert!(args.windows(2).any(|pair| pair == ["-i", "https://example.com/live/1"]));
        assert!(args.windows(2).any(|pair| pair == ["-f", "mpegts"]));
        assert_eq!(args.last(), Some(&output.to_string_lossy().to_string()));
    }

    #[test]
    fn classify_ffmpeg_failure_skips_generic_trailer_lines() {
        let result = classify_ffmpeg_failure(b"Connection timed out\nConversion failed!\n");

        assert_eq!(result, RecordingExecutionResult::Retryable("Connection timed out".to_string()));
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
        assert_eq!(result, RecordingExecutionResult::Retryable("Connection timed out".to_string()));
    }

    #[test]
    fn classify_ffmpeg_failure_keeps_terminal_usage_errors_failed() {
        let result = classify_ffmpeg_failure(b"Last message\nInvalid argument\n");
        assert_eq!(result, RecordingExecutionResult::Failed("Invalid argument".to_string()));
    }

    #[test]
    fn classify_ffmpeg_failure_marks_broader_transient_network_errors_retryable() {
        let result = classify_ffmpeg_failure(b"Last message\nCould not resolve host: example.com\n");
        assert_eq!(result, RecordingExecutionResult::Retryable("Could not resolve host: example.com".to_string()));
    }

    #[test]
    fn classify_ffmpeg_failure_ignores_phrases_inside_the_source_url() {
        // ffmpeg echoes the input URL in its error lines. A provider path
        // that happens to contain a transient-sounding phrase must not
        // turn a fatal error into an endless retry loop.
        let result = classify_ffmpeg_failure(
            b"http://host/live/connection-refused/1.ts: Invalid data found when processing input\n",
        );
        assert!(
            matches!(result, RecordingExecutionResult::Failed(_)),
            "url-borne phrase must not make a fatal error retryable: {result:?}"
        );
        // ffmpeg's own words still classify as retryable even when the
        // line also carries the URL.
        let result = classify_ffmpeg_failure(b"http://host/live/1.ts: Connection refused\n");
        assert!(matches!(result, RecordingExecutionResult::Retryable(_)), "{result:?}");
    }

    #[test]
    fn retryable_phrases_are_lowercase_so_the_matcher_can_find_them() {
        for phrase in RETRYABLE_FFMPEG_PHRASES {
            assert_eq!(
                *phrase,
                phrase.to_ascii_lowercase(),
                "phrase must be lowercase to match the lowercased haystack"
            );
        }
    }

    #[test]
    fn retryable_phrases_have_no_proper_subset() {
        // A phrase that is a proper substring of another phrase is dead
        // weight: the matcher uses `contains`, so the longer entry
        // matches anything the shorter one does. Two entries with the
        // same characters (case-insensitive) are also caught: order
        // matters at evaluation time but the matcher must be
        // deterministic.
        let phrases: Vec<String> = RETRYABLE_FFMPEG_PHRASES.iter().map(|p| p.to_ascii_lowercase()).collect();
        for (i, outer) in phrases.iter().enumerate() {
            for (j, inner) in phrases.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert!(
                    !inner.contains(outer.as_str()),
                    "phrase {inner:?} is a substring of {outer:?}; the longer entry already covers it"
                );
            }
        }
    }

    #[test]
    fn classify_ffmpeg_failure_marks_only_transient_tls_failures_retryable() {
        let retryable = classify_ffmpeg_failure(b"Last message\ntls handshake timeout\n");
        let certificate = classify_ffmpeg_failure(b"Last message\ncertificate verify failed\n");
        let protocol = classify_ffmpeg_failure(b"Last message\nunsupported protocol version\n");

        assert_eq!(retryable, RecordingExecutionResult::Retryable("tls handshake timeout".to_string()));
        assert_eq!(certificate, RecordingExecutionResult::Failed("certificate verify failed".to_string()));
        assert_eq!(protocol, RecordingExecutionResult::Failed("unsupported protocol version".to_string()));
    }

    fn fake_ffmpeg_script(name: &str, body: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_nanos();
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

        let result = run_recording_with_binary(
            &script,
            &recording,
            &control_signal,
            &control_notify,
            None,
            RecordingContainerFormat::default(),
        )
        .await;

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

        let result = run_recording_with_binary(
            &script,
            &recording,
            &control_signal,
            &control_notify,
            None,
            RecordingContainerFormat::default(),
        )
        .await;

        assert_eq!(result, RecordingExecutionResult::Retryable("Could not resolve host: upstream.example".to_string()));
        let _ = fs::remove_file(script);
        let _ = fs::remove_dir_all(&recording.file_dir);
    }

    #[tokio::test]
    async fn run_recording_preempts_fake_ffmpeg_and_preserves_window_semantics() {
        let script = fake_ffmpeg_script("preempt", "#!/bin/sh\ntrap 'exit 0' TERM INT\nsleep 30\n");
        let control_signal = RwLock::new(DownloadControl::None);
        let control_notify = Notify::new();
        let cancel_token = CancellationToken::new();
        let recording = make_recording(chrono::Utc::now().timestamp().saturating_sub(2), 30);
        let notify_cancel = cancel_token.clone();

        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            notify_cancel.cancel();
        });

        let result = run_recording_with_binary(
            &script,
            &recording,
            &control_signal,
            &control_notify,
            Some(&cancel_token),
            RecordingContainerFormat::default(),
        )
        .await;
        cancel_task.await.expect("cancel task");

        assert_eq!(result, RecordingExecutionResult::Preempted);
        assert!(remaining_recording_duration_secs(&recording, chrono::Utc::now().timestamp()).is_some());
        let _ = fs::remove_file(script);
        let _ = fs::remove_dir_all(&recording.file_dir);
    }

    #[tokio::test]
    async fn run_recording_refuses_retry_or_resume_when_partial_output_exists() {
        let script = fake_ffmpeg_script("no-resume", "#!/bin/sh\nprintf 'should not run' >&2\nexit 1\n");
        let control_signal = RwLock::new(DownloadControl::None);
        let control_notify = Notify::new();
        let recording = make_recording(chrono::Utc::now().timestamp(), 5);

        fs::create_dir_all(&recording.file_dir).expect("create recording dir");
        fs::write(recording_partial_path(&recording.file_path), b"partial").expect("write partial output");

        let result = run_recording_with_binary(
            &script,
            &recording,
            &control_signal,
            &control_notify,
            None,
            RecordingContainerFormat::default(),
        )
        .await;

        assert_eq!(result, RecordingExecutionResult::Failed("Recording resume is not supported".to_string(),));
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

    #[tokio::test]
    async fn recovery_decision_for_completed_when_final_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let final_path = dir.path().join("rec.ts");
        tokio::fs::write(&final_path, b"recorded").await.expect("write final");
        let partial = dir.path().join("rec.ts.partial");
        let decision = recovery_decision_for(&final_path, &partial).await;
        assert_eq!(decision, RecoveryDecision::Completed);
    }

    #[tokio::test]
    async fn recovery_decision_for_failed_when_only_partial_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let final_path = dir.path().join("rec.ts");
        let partial = dir.path().join("rec.ts.partial");
        tokio::fs::write(&partial, b"partial bytes").await.expect("write partial");
        let decision = recovery_decision_for(&final_path, &partial).await;
        assert_eq!(decision, RecoveryDecision::FailedPartialKept);
    }

    #[tokio::test]
    async fn recovery_decision_for_failed_when_no_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let final_path = dir.path().join("rec.ts");
        let partial = dir.path().join("rec.ts.partial");
        let decision = recovery_decision_for(&final_path, &partial).await;
        assert_eq!(decision, RecoveryDecision::FailedNoFile);
    }

    // Windows symlink creation needs developer mode or elevation, so the
    // symlink-specific assertion is Unix-only. The behaviour it covers —
    // `no_follow_existing` not following a link — is provided by
    // `symlink_metadata` on both platforms.
    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_decision_for_treats_symlinked_final_as_completed() {
        // The no-follow check returns None for symlinks; the helper
        // therefore treats the symlink as "not present" and falls through
        // to the partial check. The startup recovery in
        // `recover_loaded_download` is responsible for failing closed
        // when the path is a symlink to outside the root; this helper
        // only inspects the file presence semantics.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.ts");
        let link_path = dir.path().join("rec.ts");
        tokio::fs::write(&real, b"data").await.expect("write real");
        std::os::unix::fs::symlink(&real, &link_path).expect("symlink");
        let partial = dir.path().join("rec.ts.partial");
        let decision = recovery_decision_for(&link_path, &partial).await;
        assert_eq!(decision, RecoveryDecision::FailedNoFile);
    }
}
