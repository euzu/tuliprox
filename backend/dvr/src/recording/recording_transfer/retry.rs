use super::{mark_recording_metadata_notification, take_active, DownloadExecutionResult, RecordingNotificationPlan};
use crate::{
    http_transfer::{is_retryable_error, retryable_transport_error_message},
    recording::{
        recording_notification::LifecycleEvent,
        recording_queue::{mutate_optional, QueueMutationError, RecordingQueue, RecordingTaskState},
    },
};
use tuliprox_core::model::RecordingConfig;

pub(super) fn classify_download_open_error(url: &reqwest::Url, err: &reqwest::Error) -> DownloadExecutionResult {
    if is_retryable_error(err) {
        DownloadExecutionResult::Retryable(format!("Error while opening url: {url} {err}"))
    } else {
        DownloadExecutionResult::Failed(format!("Error while opening url: {url} {err}"))
    }
}

pub(super) fn classify_download_stream_io_error(file_path_str: &str, err: &std::io::Error) -> DownloadExecutionResult {
    if retryable_transport_error_message(&err.to_string()) {
        DownloadExecutionResult::Retryable(format!("Error while downloading file: {file_path_str} {err}"))
    } else {
        DownloadExecutionResult::Failed(format!("Error while downloading file: {file_path_str} {err}"))
    }
}

fn apply_download_retry_jitter(base_secs: u64, jitter_percent: u8) -> u64 {
    let jitter_percent = i64::from(jitter_percent.min(95));
    if jitter_percent == 0 {
        return base_secs.max(1);
    }
    let jitter_percent = fastrand::i64(-jitter_percent..=jitter_percent);
    let base_i64 = i64::try_from(base_secs.max(1)).unwrap_or(i64::MAX);
    let jitter_delta = base_i64.saturating_mul(jitter_percent).saturating_div(100);
    let jittered = base_i64.saturating_add(jitter_delta);
    u64::try_from(jittered.max(1)).unwrap_or(1)
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn compute_download_retry_backoff_secs(attempts: u8, download_cfg: &RecordingConfig) -> u64 {
    let exponent = i32::from(attempts.saturating_sub(1));
    let scaled_secs =
        (download_cfg.retry_backoff_initial_secs as f64) * download_cfg.retry_backoff_multiplier.powi(exponent);
    let clamped_secs =
        scaled_secs.clamp(download_cfg.retry_backoff_initial_secs as f64, download_cfg.retry_backoff_max_secs as f64);
    let base_secs = clamped_secs.round() as u64;
    apply_download_retry_jitter(base_secs, download_cfg.retry_backoff_jitter_percent)
}

pub(super) async fn requeue_active_download_for_retry(
    download_queue: &RecordingQueue,
    uuid: &str,
    promote: bool,
) -> Result<bool, QueueMutationError> {
    Ok(mutate_optional(download_queue, |candidate| {
        let Some(mut download) = take_active(candidate, uuid) else {
            return Ok(None);
        };
        download.finished = false;
        download.paused = false;
        download.error = None;
        download.state = RecordingTaskState::Queued;
        download.next_retry_at = None;
        candidate.queue.insert(0, download);
        if promote {
            crate::recording::recording_queue::promote_from_queue(candidate);
        }
        Ok(Some(true))
    })
    .await?
    .unwrap_or(false))
}

pub(super) enum RetryCommit {
    Waiting { delay_secs: u64, attempts: u8 },
    Failed(RecordingNotificationPlan),
}

pub(super) async fn prepare_active_retry(
    download_queue: &RecordingQueue,
    uuid: &str,
    download_cfg: &RecordingConfig,
    cause: &str,
) -> Result<Option<RetryCommit>, QueueMutationError> {
    mutate_optional(download_queue, |candidate| {
        let Some(active) = candidate.active.iter_mut().find(|active| active.uuid == uuid) else {
            return Ok(None);
        };
        // A live broadcast cannot be retried: whatever played during the
        // backoff is gone, so a second attempt records a different part of the
        // programme and calls it the same recording. `RetryWaiting` is not a
        // state a live capture can legally be in either.
        let retryable_kind = active.kind.is_resumable();
        active.retry_attempts = active.retry_attempts.saturating_add(1);
        let attempts = active.retry_attempts;
        if !retryable_kind || attempts > download_cfg.retry_max_attempts {
            let Some(mut failed) = take_active(candidate, uuid) else {
                return Ok(None);
            };
            let error = if retryable_kind {
                format!("Retry limit reached after {} attempts: {cause}", download_cfg.retry_max_attempts)
            } else {
                format!("Live recording failed; restarting a live capture is not supported: {cause}")
            };
            failed.finished = true;
            failed.paused = false;
            failed.next_retry_at = None;
            failed.state = RecordingTaskState::Failed;
            failed.error = Some(error.clone());
            failed.recording.reserved_bytes = 0;
            let notification =
                mark_recording_metadata_notification(&mut failed.recording, LifecycleEvent::Failed, Some(error));
            candidate.finished.push(failed);
            crate::recording::recording_queue::promote_from_queue(candidate);
            return Ok(Some(RetryCommit::Failed(notification)));
        }

        let delay_secs = compute_download_retry_backoff_secs(attempts, download_cfg);
        let next_retry_at =
            chrono::Utc::now().timestamp().saturating_add(i64::try_from(delay_secs).unwrap_or(i64::MAX));
        active.next_retry_at = Some(next_retry_at);
        active.state = RecordingTaskState::RetryWaiting;
        active.paused = false;
        active.finished = false;
        active.error = Some(format!(
            "Retrying after transient failure in {delay_secs}s (attempt {attempts}/{}): {cause}",
            download_cfg.retry_max_attempts
        ));
        Ok(Some(RetryCommit::Waiting { delay_secs, attempts }))
    })
    .await
}
