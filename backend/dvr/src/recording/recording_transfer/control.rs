use super::{DownloadExecutionResult, DOWNLOAD_PREEMPTED_REASON, RECORDING_PREEMPTED_REASON};
use crate::recording::recording_queue::{RecordingControl, RecordingQueue, RecordingTask};
use shared::model::RecordingKind;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::RwLock,
    time::{Duration, Instant},
};

pub(super) fn current_download_control(control_signal: &RwLock<RecordingControl>) -> RecordingControl {
    control_signal.try_read().map_or(RecordingControl::None, |control| *control)
}

pub(super) fn should_exit_worker_after_preempt(control: RecordingControl) -> bool {
    control == RecordingControl::Restart
}

/// Close the writer for a pause, cancel or restart, so everything received
/// so far is on disk before the worker reports the outcome.
pub(super) async fn handle_download_control<W>(
    control: RecordingControl,
    buf_writer: &mut W,
) -> Option<DownloadExecutionResult>
where
    W: AsyncWrite + Unpin,
{
    let outcome = handle_download_control_without_writer(control)?;
    let closed = async {
        buf_writer.flush().await?;
        buf_writer.shutdown().await
    };
    if let Err(err) = closed.await {
        return Some(DownloadExecutionResult::Failed(err.to_string()));
    }
    Some(outcome)
}

pub(super) fn handle_download_control_without_writer(control: RecordingControl) -> Option<DownloadExecutionResult> {
    match control {
        RecordingControl::Pause => Some(DownloadExecutionResult::Paused),
        RecordingControl::Cancel => Some(DownloadExecutionResult::Cancelled),
        RecordingControl::Restart => Some(DownloadExecutionResult::Preempted),
        RecordingControl::None => None,
    }
}

pub(super) fn recording_deadline_instant(task: &RecordingTask) -> Option<Instant> {
    if task.kind != RecordingKind::Live {
        return None;
    }
    let (start_at, duration_secs) = task.scheduled_start().zip(task.scheduled_duration_secs())?;
    let deadline_ts = start_at.saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX));
    let now_ts = chrono::Utc::now().timestamp();
    if now_ts >= deadline_ts {
        return Some(Instant::now());
    }
    let remaining_secs = u64::try_from(deadline_ts.saturating_sub(now_ts)).ok()?;
    Some(Instant::now() + Duration::from_secs(remaining_secs))
}

pub(super) async fn continue_after_pause(download_queue: &RecordingQueue, uuid: &str) -> bool {
    download_queue.active.read().await.iter().any(|task| task.uuid == uuid && !task.paused && !task.finished)
}

pub(super) fn preemption_reason_for(download: &RecordingTask) -> &'static str {
    match download.kind {
        RecordingKind::Vod | RecordingKind::Series => DOWNLOAD_PREEMPTED_REASON,
        RecordingKind::Live => RECORDING_PREEMPTED_REASON,
    }
}
