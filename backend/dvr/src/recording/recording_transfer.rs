//! Recording execution engine.
//!
//! Owns everything that happens after a task is queued: provider-slot
//! acquisition, the resumable HTTP transfer loop for VOD/Series, the ffmpeg
//! strategy for Live, retry/backoff, queue state transitions, lifecycle
//! notifications and progress publication. The HTTP layer only queues tasks
//! and reads the queue; it never drives execution.

use tokio::time::Duration;

const DOWNLOAD_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(5);

const DOWNLOAD_PROGRESS_LOG_BYTES: u64 = 16 * 1024 * 1024;

const DOWNLOAD_SNAPSHOT_UPDATE_INTERVAL: Duration = Duration::from_secs(2);

const DOWNLOAD_SNAPSHOT_UPDATE_BYTES: u64 = 4 * 1024 * 1024;

// Pause/cancel/restart are delivered immediately via `control_notify` while the
// worker is parked in the `select!`. This poll is only a fallback for the rare
// race where a control change fires while a chunk is being written (notify is not
// persisted), so it does not need to run on every chunk.
const DOWNLOAD_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(200);

pub(crate) const RANGE_UNSUPPORTED_ERROR: &str = "range_unsupported";

const RECORDING_PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_secs(5);

const LIVE_CAPACITY_WINDOW_CLOSED: &str = "No provider capacity became available before the recording window closed";

const DOWNLOAD_PREEMPTED_REASON: &str = "Preempted by higher-priority foreground stream";

const RECORDING_PREEMPTED_REASON: &str =
    "Recording preempted by higher-priority foreground stream; waiting to resume within the remaining window";

pub(crate) const QUOTA_EXCEEDED_DURING_TRANSFER: &str =
    "Quota exceeded: the recording is larger than the quota that is left";

const SIBLING_QUOTA_EXCEEDED: &str = "Quota exceeded: the recording is larger than this entry's quota allows";

pub(crate) const QUOTA_GONE_BEFORE_START: &str = "Quota was exhausted while this recording waited to start";

pub(crate) const DISK_GONE_BEFORE_START: &str = "Disk space was exhausted while this recording waited to start";

#[cfg(test)]
mod tests;

mod completion;
mod control;
mod http;
mod notifications;
mod provider;
mod quota;
mod retry;
mod worker;

#[cfg(test)]
use self::http::finalize_http_transfer;
#[cfg(test)]
use self::http::http_transfer_path;
#[cfg(test)]
use self::http::send_download_request;
#[cfg(test)]
use self::worker::requeue_active_download_for_capacity_wait;
#[cfg(test)]
use self::worker::start_recording_scheduler;
pub use self::worker::{ensure_recording_worker_running, resume_recording_worker_if_needed, spawn_recording_services};
use self::{
    completion::{
        active_download_snapshot_for_worker, cancel_active_and_promote, fail_active_download,
        finish_active_and_promote, promote_ready_downloads, set_active_download_state, take_active,
        update_active_download_for_worker, write_completion_sidecar,
    },
    control::{
        continue_after_pause, current_download_control, handle_download_control,
        handle_download_control_without_writer, preemption_reason_for, recording_deadline_instant,
        should_exit_worker_after_preempt,
    },
    http::{download_file, recording_execution_download},
    notifications::{
        broadcast_required_worker_mutation, broadcast_worker_mutation, mark_recording_metadata_notification,
        publish_recording_change, publish_recording_progress, refresh_recording_progress,
        spawn_recording_notification_after_persist, RecordingNotificationPlan,
    },
    provider::{
        acquire_result_after_wait, acquire_result_for_control, background_download_should_wait,
        capacities_have_free_slot, commit_acquired_download, wait_for_provider_slot, ProviderAcquireResult,
        ProviderCapacities,
    },
    quota::{charge_waiting_siblings, refused_before_start, QuotaGate},
    retry::{
        classify_download_open_error, classify_download_stream_io_error, prepare_active_retry,
        requeue_active_download_for_retry, RetryCommit,
    },
    worker::DownloadExecutionResult,
};
