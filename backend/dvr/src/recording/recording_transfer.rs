//! Recording execution engine.
//!
//! Owns everything that happens after a task is queued: provider-slot
//! acquisition, the resumable HTTP transfer loop for VOD/Series, the ffmpeg
//! strategy for Live, retry/backoff, queue state transitions, lifecycle
//! notifications and progress publication. The HTTP layer only queues tasks
//! and reads the queue; it never drives execution.

use crate::{
    http_transfer::{
        compute_total_size, is_retryable_error, is_retryable_status, retryable_transport_error_message,
        validate_resume_response, ResponseSnapshot, ResumeValidationError, ResumeValidator,
    },
    recording::{
        recording_capacity::RecordingCapacityPort,
        recording_ctx::RecordingCtx,
        recording_notification::LifecycleEvent,
        recording_notification_adapter::{build_marker, decide, message_for, DispatchDecision},
        recording_queue::{
            mutate_optional, PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError, RecordingControl,
            RecordingQueue, RecordingTask, RecordingTaskState, RecordingWaitOutcome,
        },
        recording_sidecar,
        recording_url::build_stable_recording_url,
        recording_worker::{
            recording_partial_path, redact_url_tokens, run_recording_with_binary, RecordingExecutionResult,
        },
    },
};
use futures::stream::TryStreamExt;
use log::{debug, error, info, warn};
use shared::{
    error::to_io_error,
    model::{EventMessage, EventSink, RecordingKind, RecordingMetadata},
    utils::bytes_to_megabytes,
};
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::{
    fs,
    io::{AsyncWrite, AsyncWriteExt},
    sync::{Notify, RwLock},
    time::{self, Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{AppConfig, MessageContent, RecordingConfig},
    utils::{async_file_writer, request, request::create_client, IO_BUFFER_SIZE},
};

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
type ProviderCapacities = Vec<(Arc<str>, usize, usize)>;

#[derive(Debug)]
enum DownloadExecutionResult {
    Completed,
    Paused,
    Cancelled,
    Preempted,
    Retryable(String),
    Failed(String),
}

enum ProviderAcquireResult {
    Acquired(Option<tuliprox_core::model::ProviderHandle>),
    Paused,
    Cancelled,
    Preempted,
    /// A live capture waited for capacity until its broadcast window closed.
    WindowClosed,
}

fn recording_execution_download(
    app_config: &AppConfig,
    task: &RecordingTask,
    provider_handle: Option<&tuliprox_core::model::ProviderHandle>,
) -> Result<RecordingTask, String> {
    let source = &task.recording.source;
    let virtual_id = source.virtual_id.parse::<u32>().map_err(|_| "Recording source virtual id invalid".to_string())?;
    let url = build_stable_recording_url(
        app_config,
        &source.target_id,
        &source.input_name,
        virtual_id,
        source.cluster,
        provider_handle.map(|handle| handle.allocation_id),
    )
    .ok_or_else(|| "Recording execution URL unavailable".to_string())?;
    let mut execution = task.clone();
    execution.url = reqwest::Url::parse(&url).map_err(|_| "Recording execution URL invalid".to_string())?;
    Ok(execution)
}

fn classify_download_open_error(url: &reqwest::Url, err: &reqwest::Error) -> DownloadExecutionResult {
    if is_retryable_error(err) {
        DownloadExecutionResult::Retryable(format!("Error while opening url: {url} {err}"))
    } else {
        DownloadExecutionResult::Failed(format!("Error while opening url: {url} {err}"))
    }
}

fn classify_download_stream_io_error(file_path_str: &str, err: &std::io::Error) -> DownloadExecutionResult {
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

fn background_download_should_wait(
    priority: i8,
    capacities: &[(Arc<str>, usize, usize)],
    download_cfg: &RecordingConfig,
) -> bool {
    if priority <= 0 || capacities.is_empty() {
        return false;
    }

    let background_limit = usize::from(download_cfg.max_background_per_provider);
    let reserve_slots = usize::from(download_cfg.reserve_slots_for_users);

    let blocked_by_background_limit =
        background_limit > 0 && capacities.iter().all(|(_, current, _)| *current >= background_limit);
    let blocked_by_reserved_slots = reserve_slots > 0
        && capacities.iter().all(|(_, current, max)| *max > 0 && current.saturating_add(reserve_slots) >= *max);

    blocked_by_background_limit || blocked_by_reserved_slots
}

fn capacities_have_free_slot(capacities: &[(Arc<str>, usize, usize)]) -> bool {
    capacities.iter().any(|(_, current, max)| *max == 0 || current < max)
}

async fn active_download_snapshot_for_worker(
    active: &RwLock<Vec<RecordingTask>>,
    worker_uuid: &str,
) -> Option<RecordingTask> {
    active.read().await.iter().find(|task| task.uuid == worker_uuid).cloned()
}

async fn update_active_download_for_worker<F>(active: &RwLock<Vec<RecordingTask>>, worker_uuid: &str, update: F) -> bool
where
    F: FnOnce(&mut RecordingTask) -> bool,
{
    let mut active = active.write().await;
    let Some(task) = active.iter_mut().find(|task| task.uuid == worker_uuid) else {
        return false;
    };
    update(task)
}

/// Publish a queue change. Sessions answer it by pulling an owner-filtered
/// snapshot; no task data is broadcast globally, so a session can never see
/// another user's recording.
fn publish_recording_change<E: EventSink>(event_manager: &E) { event_manager.emit(EventMessage::RecordingChanged); }

/// Announce that a running recording has grown.
///
/// A separate event from [`publish_recording_change`]: a capture can produce
/// hundreds of these a second, and a session throttles them. A state
/// transition must never be throttled, so it does not come through here.
fn publish_recording_progress<E: EventSink>(event_manager: &E) { event_manager.emit(EventMessage::RecordingProgress); }

fn broadcast_worker_mutation<E: EventSink>(
    event_manager: &E,
    result: Result<bool, QueueMutationError>,
    action: &str,
) -> Result<bool, QueueMutationError> {
    match result {
        Ok(true) => {
            publish_recording_change(event_manager);
            Ok(true)
        }
        Ok(false) => Ok(false),
        Err(err) => Err(QueueMutationError::new(format!("{action}: {err}"))),
    }
}

fn broadcast_required_worker_mutation<E: EventSink>(
    event_manager: &E,
    result: Result<bool, QueueMutationError>,
    action: &str,
) -> Result<(), QueueMutationError> {
    if broadcast_worker_mutation(event_manager, result, action)? {
        Ok(())
    } else {
        Err(QueueMutationError::new(format!("{action}: active task changed")))
    }
}

async fn refresh_recording_progress<E: EventSink>(
    active: &RwLock<Vec<RecordingTask>>,
    worker_uuid: &str,
    file_path: &std::path::Path,
    event_manager: &E,
) {
    let current_size = tokio::fs::metadata(file_path).await.map_or(0, |metadata| metadata.len());
    let changed = update_active_download_for_worker(active, worker_uuid, |task| {
        if task.kind == RecordingKind::Live && task.size != current_size {
            task.size = current_size;
            true
        } else {
            false
        }
    })
    .await;
    if changed {
        publish_recording_progress(event_manager);
    }
}

/// The validators a later resume can be checked against.
///
/// A weak `ETag` is discarded: it only promises semantic equivalence, so it
/// cannot prove the bytes on the far side of an interruption are the same
/// ones. `Last-Modified` is the fallback, and is only meaningful when no
/// strong tag was offered.
fn capture_resume_validators(response: &reqwest::Response) -> (Option<String>, Option<String>) {
    let headers = response.headers();
    let etag = headers
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .and_then(crate::http_transfer::strong_etag)
        .map(str::to_owned);
    if etag.is_some() {
        return (etag, None);
    }
    let last_modified = headers
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    (None, last_modified)
}

async fn send_download_request(
    client: &reqwest::Client,
    upstream_client: Option<&reqwest::Client>,
    url: &reqwest::Url,
    offset: u64,
    control_signal: &RwLock<RecordingControl>,
    control_notify: &Notify,
) -> Result<reqwest::Response, DownloadExecutionResult> {
    if let Some(result) = handle_download_control_without_writer(current_download_control(control_signal)) {
        return Err(result);
    }

    let send = async {
        let mut destination = url.clone();
        let mut upstream_origin = None;
        for redirects in 0..=10 {
            let hop_client =
                if destination.origin() == url.origin() { client } else { upstream_client.unwrap_or(client) };
            // Resume offsets refer to bytes on disk, so every hop must preserve encoding.
            let mut request = hop_client
                .get(destination.clone())
                .header(reqwest::header::USER_AGENT, shared::model::RECORDING_STREAM_USER_AGENT)
                .header(reqwest::header::ACCEPT_ENCODING, "identity");
            if offset > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            }
            if destination.origin() != url.origin() {
                let trusted_origin = upstream_origin.get_or_insert_with(|| destination.origin());
                if upstream_client.is_none() || destination.origin() != *trusted_origin {
                    // Listener credentials stay local; provider credentials stay
                    // on the first upstream origin throughout the redirect chain.
                    request = request.header(reqwest::header::AUTHORIZATION, "").header(reqwest::header::COOKIE, "");
                }
            }
            let response = request.send().await.map_err(|error| classify_download_open_error(&destination, &error))?;
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                return Ok(response);
            };
            let next = location
                .to_str()
                .ok()
                .and_then(|value| response.url().join(value).ok())
                .filter(|value| matches!(value.scheme(), "http" | "https"));
            let Some(next) = next else {
                return Err(DownloadExecutionResult::Failed("Invalid recording redirect location".to_string()));
            };
            if redirects == 10 {
                return Err(DownloadExecutionResult::Failed("Recording redirect limit exceeded".to_string()));
            }
            destination = next;
        }
        Err(DownloadExecutionResult::Failed("Recording redirect limit exceeded".to_string()))
    };
    tokio::pin!(send);
    loop {
        tokio::select! {
            biased;
            () = control_notify.notified() => {
                if let Some(result) = handle_download_control_without_writer(*control_signal.read().await) {
                    return Err(result);
                }
            }
            response = &mut send => return response,
        }
    }
}

/// Wait for a provider slot, giving up when a live window closes.
///
/// Waiting past `scheduled_end` cannot produce the recording that was asked
/// for: the programme has finished. Only live work has a deadline; a transfer
/// waits as long as it takes.
async fn wait_for_provider_slot(
    download_queue: &RecordingQueue,
    input_name: &Arc<str>,
    priority: i8,
    control_signal: &RwLock<RecordingControl>,
    control_notify: &Notify,
    deadline: Option<Instant>,
) -> Option<RecordingWaitOutcome> {
    let waiting =
        download_queue.slot_waiters.wait(Some(Arc::clone(input_name)), priority, control_signal, control_notify);
    match deadline {
        Some(deadline) => time::timeout_at(deadline, waiting).await.ok(),
        None => Some(waiting.await),
    }
}

/// What a control signal observed while acquiring a slot means; `None` keeps
/// trying.
fn acquire_result_for_control(control: RecordingControl) -> Option<ProviderAcquireResult> {
    match control {
        RecordingControl::Cancel => Some(ProviderAcquireResult::Cancelled),
        RecordingControl::Pause => Some(ProviderAcquireResult::Paused),
        RecordingControl::Restart => Some(ProviderAcquireResult::Preempted),
        RecordingControl::None => None,
    }
}

/// What the end of a provider-slot wait means; `None` (signalled) tries to
/// acquire again. A wait without an outcome ran into the live window's end.
fn acquire_result_after_wait(outcome: Option<RecordingWaitOutcome>) -> Option<ProviderAcquireResult> {
    match outcome {
        None => Some(ProviderAcquireResult::WindowClosed),
        Some(RecordingWaitOutcome::Signalled) => None,
        Some(RecordingWaitOutcome::Paused) => Some(ProviderAcquireResult::Paused),
        Some(RecordingWaitOutcome::Cancelled) => Some(ProviderAcquireResult::Cancelled),
        Some(RecordingWaitOutcome::Restarted) => Some(ProviderAcquireResult::Preempted),
    }
}

fn http_transfer_path(task: &RecordingTask) -> std::path::PathBuf {
    if task.kind.is_resumable() {
        recording_partial_path(&task.file_path)
    } else {
        task.file_path.clone()
    }
}

/// Publish a completed transfer at its final path.
///
/// Idempotent: linking is the only step that is not naturally repeatable. A
/// crash between the link and the partial's removal leaves the final file in
/// place, and finalizing again accepts it instead of failing the recording.
async fn finalize_http_transfer(final_path: &std::path::Path, transfer_path: &std::path::Path) -> std::io::Result<()> {
    if transfer_path == final_path {
        return Ok(());
    }
    fs::File::open(transfer_path).await?.sync_all().await?;
    match fs::hard_link(transfer_path, final_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Path reservation gives this recording sole claim on `final_path`,
            // so something already there is this task's own earlier link. A
            // size mismatch means that reasoning does not hold; say so rather
            // than publish bytes that were never checked.
            let published = fs::metadata(final_path).await?.len();
            let staged = fs::metadata(transfer_path).await?.len();
            if published != staged {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "{} already exists with {published} bytes, but the transfer staged {staged}",
                        final_path.display()
                    ),
                ));
            }
        }
        Err(error) => return Err(error),
    }
    if let Err(error) = fs::remove_file(transfer_path).await {
        warn!("Finalized {} but could not remove partial {}: {error}", final_path.display(), transfer_path.display());
    }
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn download_file<E: EventSink>(
    active: Arc<RwLock<Vec<RecordingTask>>>,
    file_download: RecordingTask,
    client: &reqwest::Client,
    upstream_client: Option<&reqwest::Client>,
    control_signal: Arc<RwLock<RecordingControl>>,
    control_notify: Arc<Notify>,
    event_manager: Option<&E>,
    quota_gate: Option<QuotaGate<'_>>,
) -> DownloadExecutionResult {
    let worker_uuid = file_download.uuid.as_str();
    let url = file_download.url.clone();
    let file_path = http_transfer_path(&file_download);
    let existing_size = tokio::fs::metadata(&file_path).await.map_or(0, |metadata| metadata.len());
    let response_result =
        send_download_request(client, upstream_client, &url, existing_size, &control_signal, &control_notify).await;

    match response_result {
        Ok(response) => {
            if existing_size > 0 {
                // The validators were captured when the first byte was written.
                // Without them a provider that replaces the file between an
                // interruption and the resume returns a well-formed 206 whose
                // bytes belong to a different resource, and they are appended
                // to the old partial.
                let validator = ResumeValidator {
                    expected_offset: existing_size,
                    expected_total: file_download.total_size,
                    expected_etag: file_download.recording.resume_etag.clone(),
                    expected_last_modified: file_download.recording.resume_last_modified.clone(),
                };
                match validate_resume_response(&ResponseSnapshot::from_response(&response), &validator) {
                    Ok(()) => {}
                    Err(ResumeValidationError::Unsatisfiable { complete: true }) => {
                        return match finalize_http_transfer(&file_download.file_path, &file_path).await {
                            Ok(()) => DownloadExecutionResult::Completed,
                            Err(error) => DownloadExecutionResult::Failed(format!(
                                "Could not finalize {}: {error}",
                                file_download.file_path.display()
                            )),
                        };
                    }
                    Err(ResumeValidationError::IgnoredRange) => {
                        return DownloadExecutionResult::Failed(RANGE_UNSUPPORTED_ERROR.to_string());
                    }
                    Err(error) => {
                        return DownloadExecutionResult::Failed(format!(
                            "Cannot resume download at byte {existing_size} for {url}: {error}; partial file preserved"
                        ));
                    }
                }
            }
            let status = response.status();
            if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                if is_retryable_status(status) {
                    return DownloadExecutionResult::Retryable(format!(
                        "Download request failed for {url} with transient HTTP {status}"
                    ));
                }
                return DownloadExecutionResult::Failed(format!(
                    "Download request failed for {url} with HTTP {status}"
                ));
            }
            let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

            let total_size = compute_total_size(&response, existing_size);
            let captured = capture_resume_validators(&response);

            if total_size.is_some() || existing_size == 0 {
                update_active_download_for_worker(&active, worker_uuid, |download| {
                    if let Some(total) = total_size {
                        download.total_size = Some(total);
                    }
                    // Only a fresh transfer may set the validators; a resume
                    // must keep the ones its partial was written against.
                    if existing_size == 0 {
                        download.recording.resume_etag.clone_from(&captured.0);
                        download.recording.resume_last_modified.clone_from(&captured.1);
                    }
                    true
                })
                .await;
            }

            // The size is known from here on, or known to be unknown.
            let byte_cap = match quota_gate {
                Some(gate) => match gate.admit_size(worker_uuid, total_size, existing_size).await {
                    Ok(admitted) => {
                        if admitted.siblings_refused {
                            if let Some(event_manager) = event_manager {
                                publish_recording_change(event_manager);
                            }
                        }
                        admitted.byte_cap
                    }
                    Err(reason) => return DownloadExecutionResult::Failed(reason),
                },
                None => None,
            };

            match fs::create_dir_all(&file_download.file_dir).await {
                Ok(()) => {
                    let mut open_options = tokio::fs::OpenOptions::new();
                    let file_mode = if existing_size > 0 && is_resume {
                        open_options.append(true)
                    } else {
                        open_options.write(true).create(true).truncate(true)
                    };

                    if let Some(file_path_str) = file_path.to_str() {
                        info!("{} {}", if is_resume { "Resuming" } else { "Downloading" }, file_path_str);
                        match file_mode.open(&file_path).await {
                            Ok(file) => {
                                let mut buf_writer = async_file_writer(file);
                                let mut downloaded: u64 = if is_resume { existing_size } else { 0 };
                                let mut stream = response.bytes_stream();
                                let mut write_counter = 0;
                                let mut saw_first_chunk = existing_size > 0;
                                let mut last_progress_log_at = Instant::now();
                                let mut last_progress_logged_bytes = downloaded;
                                let mut last_snapshot_update_at = Instant::now();
                                let mut last_snapshot_update_bytes = downloaded;
                                let mut control_poll = time::interval(DOWNLOAD_CONTROL_POLL_INTERVAL);
                                control_poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                                control_poll.tick().await;
                                loop {
                                    let next_item = tokio::select! {
                                        biased;
                                        () = control_notify.notified() => {
                                            if let Some(result) =
                                                handle_download_control(*control_signal.read().await, &mut buf_writer)
                                                    .await
                                            {
                                                return result;
                                            }
                                            continue;
                                        }
                                        _ = control_poll.tick() => {
                                            if let Some(result) = handle_download_control(
                                                current_download_control(&control_signal),
                                                &mut buf_writer,
                                            )
                                            .await
                                            {
                                                return result;
                                            }
                                            continue;
                                        }
                                        next_item = stream.try_next() => next_item.map_err(to_io_error),
                                    };

                                    match next_item {
                                        Ok(item) => {
                                            if let Some(chunk) = item {
                                                match buf_writer.write_all(&chunk).await {
                                                    Ok(()) => {
                                                        write_counter += chunk.len();
                                                        if write_counter >= IO_BUFFER_SIZE {
                                                            if let Err(err) = buf_writer.flush().await {
                                                                return DownloadExecutionResult::Failed(
                                                                    err.to_string(),
                                                                );
                                                            }
                                                            write_counter = 0;
                                                        }

                                                        downloaded += chunk.len() as u64;
                                                        if byte_cap.is_some_and(|cap| downloaded > cap) {
                                                            // Unknown size, so the quota is enforced as
                                                            // the bytes arrive.
                                                            let _ = buf_writer.flush().await;
                                                            let _ = buf_writer.shutdown().await;
                                                            return DownloadExecutionResult::Failed(
                                                                QUOTA_EXCEEDED_DURING_TRANSFER.to_string(),
                                                            );
                                                        }
                                                        if saw_first_chunk {
                                                            let now = Instant::now();
                                                            let should_log_progress = now
                                                                .duration_since(last_progress_log_at)
                                                                >= DOWNLOAD_PROGRESS_LOG_INTERVAL
                                                                || downloaded
                                                                    .saturating_sub(last_progress_logged_bytes)
                                                                    >= DOWNLOAD_PROGRESS_LOG_BYTES;
                                                            if should_log_progress {
                                                                match total_size {
                                                                    Some(total) if total > 0 => {
                                                                        let percent = downloaded
                                                                            .saturating_mul(100)
                                                                            .checked_div(total)
                                                                            .unwrap_or(0)
                                                                            .min(100);
                                                                        debug!(
                                                                                "Download progress for {file_path_str}: {}MB / {}MB ({}%)",
                                                                                bytes_to_megabytes(downloaded),
                                                                                bytes_to_megabytes(total),
                                                                                percent
                                                                            );
                                                                    }
                                                                    _ => {
                                                                        debug!(
                                                                                "Download progress for {file_path_str}: {}MB received",
                                                                                bytes_to_megabytes(downloaded)
                                                                            );
                                                                    }
                                                                }
                                                                last_progress_log_at = now;
                                                                last_progress_logged_bytes = downloaded;
                                                            }
                                                        } else {
                                                            saw_first_chunk = true;
                                                            info!(
                                                                    "Receiving download data for {file_path_str}: {}MB received",
                                                                    bytes_to_megabytes(downloaded)
                                                                );
                                                            last_progress_log_at = Instant::now();
                                                            last_progress_logged_bytes = downloaded;
                                                        }
                                                        let should_update_snapshot = (last_snapshot_update_bytes == 0
                                                            && downloaded > 0)
                                                            || downloaded.saturating_sub(last_snapshot_update_bytes)
                                                                >= DOWNLOAD_SNAPSHOT_UPDATE_BYTES
                                                            || Instant::now().duration_since(last_snapshot_update_at)
                                                                >= DOWNLOAD_SNAPSHOT_UPDATE_INTERVAL;
                                                        if should_update_snapshot {
                                                            let changed = update_active_download_for_worker(
                                                                &active,
                                                                worker_uuid,
                                                                |download| {
                                                                    download.size = downloaded;
                                                                    true
                                                                },
                                                            )
                                                            .await;
                                                            if changed {
                                                                if let Some(event_manager) = event_manager {
                                                                    publish_recording_progress(event_manager);
                                                                }
                                                            }
                                                            last_snapshot_update_at = Instant::now();
                                                            last_snapshot_update_bytes = downloaded;
                                                        }
                                                    }
                                                    Err(err) => {
                                                        return DownloadExecutionResult::Failed(format!(
                                                            "Error while writing to file: {file_path_str} {err}"
                                                        ));
                                                    }
                                                }
                                            } else {
                                                let megabytes = bytes_to_megabytes(downloaded);
                                                info!("Downloaded {file_path_str}, filesize: {megabytes}MB");
                                                update_active_download_for_worker(&active, worker_uuid, |download| {
                                                    download.size = downloaded;
                                                    true
                                                })
                                                .await;
                                                if let Err(err) = buf_writer.flush().await {
                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                }
                                                if let Err(err) = buf_writer.shutdown().await {
                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                }
                                                return match finalize_http_transfer(
                                                    &file_download.file_path,
                                                    &file_path,
                                                )
                                                .await
                                                {
                                                    Ok(()) => DownloadExecutionResult::Completed,
                                                    Err(error) => DownloadExecutionResult::Failed(format!(
                                                        "Could not finalize {}: {error}",
                                                        file_download.file_path.display()
                                                    )),
                                                };
                                            }
                                        }
                                        Err(err) => return classify_download_stream_io_error(file_path_str, &err),
                                    }
                                }
                            }
                            Err(err) => DownloadExecutionResult::Failed(format!(
                                "Error while opening file: {file_path_str} {err}"
                            )),
                        }
                    } else {
                        DownloadExecutionResult::Failed("Error file-download file-path unknown".to_string())
                    }
                }
                Err(err) => DownloadExecutionResult::Failed(format!(
                    "Error while creating directory for file: {} {}",
                    file_download.file_dir.to_str().unwrap_or("?"),
                    err
                )),
            }
        }
        Err(result) => result,
    }
}

fn current_download_control(control_signal: &RwLock<RecordingControl>) -> RecordingControl {
    control_signal.try_read().map_or(RecordingControl::None, |control| *control)
}

fn should_exit_worker_after_preempt(control: RecordingControl) -> bool { control == RecordingControl::Restart }

/// Close the writer for a pause, cancel or restart, so everything received
/// so far is on disk before the worker reports the outcome.
async fn handle_download_control<W>(control: RecordingControl, buf_writer: &mut W) -> Option<DownloadExecutionResult>
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

fn handle_download_control_without_writer(control: RecordingControl) -> Option<DownloadExecutionResult> {
    match control {
        RecordingControl::Pause => Some(DownloadExecutionResult::Paused),
        RecordingControl::Cancel => Some(DownloadExecutionResult::Cancelled),
        RecordingControl::Restart => Some(DownloadExecutionResult::Preempted),
        RecordingControl::None => None,
    }
}

fn recording_deadline_instant(task: &RecordingTask) -> Option<Instant> {
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

async fn set_active_download_state(
    download_queue: &RecordingQueue,
    uuid: &str,
    state: RecordingTaskState,
    error: Option<String>,
    paused: bool,
) -> Result<bool, QueueMutationError> {
    Ok(mutate_optional(download_queue, |candidate| {
        let Some(task) = candidate.active.iter_mut().find(|active| active.uuid == uuid) else {
            return Ok(None);
        };
        if task.state == state && task.error == error && task.paused == paused && !task.finished {
            return Ok(None);
        }
        task.state = state;
        task.error = error;
        task.paused = paused;
        task.finished = false;
        Ok(Some(true))
    })
    .await?
    .unwrap_or(false))
}

async fn continue_after_pause(download_queue: &RecordingQueue, uuid: &str) -> bool {
    download_queue.active.read().await.iter().any(|task| task.uuid == uuid && !task.paused && !task.finished)
}

async fn commit_acquired_download(
    download_queue: &RecordingQueue,
    uuid: &str,
) -> Result<Option<RecordingNotificationPlan>, QueueMutationError> {
    mutate_optional(download_queue, |candidate| {
        let Some(active) = candidate.active.iter_mut().find(|active| active.uuid == uuid) else {
            return Ok(None);
        };
        active.state = RecordingTaskState::Running;
        active.error = None;
        active.paused = false;
        active.finished = false;
        let notification = mark_recording_metadata_notification(&mut active.recording, LifecycleEvent::Started, None);
        Ok(Some(notification))
    })
    .await
}

fn mark_recording_metadata_notification(
    meta: &mut RecordingMetadata,
    event: LifecycleEvent,
    failure_reason: Option<String>,
) -> RecordingNotificationPlan {
    let is_admin_owner = meta.owner.user_id().is_builtin_admin();
    match decide(meta, event, chrono::Utc::now().timestamp(), is_admin_owner, failure_reason) {
        DispatchDecision::PersistAndDeliver { payload, kind, attempted_at } => {
            meta.notification_markers.push(build_marker(kind.clone(), attempted_at));
            RecordingNotificationPlan {
                message: Some(MessageContent::RecordingLifecycle(message_for(event, &payload))),
            }
        }
        DispatchDecision::AlreadyDelivered { .. } | DispatchDecision::Suppressed { .. } => {
            RecordingNotificationPlan::empty()
        }
    }
}

struct RecordingNotificationPlan {
    message: Option<MessageContent>,
}

impl RecordingNotificationPlan {
    fn empty() -> Self { Self { message: None } }
}

/// Hand a lifecycle notification off for delivery once its marker has
/// been persisted.
///
/// The marker is written inside the queue-mutation boundary, so it is
/// durable before this runs; delivery must be durable too. The
/// notification outbox owns that: it persists the entry, retries per
/// channel with backoff, and dead-letters what it cannot deliver.
///
/// A direct send is the fallback for paths that run before the supervisor
/// installs the outbox (notably unit tests), and for an outbox that refuses
/// the entry.
fn spawn_recording_notification_after_persist(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    plan: RecordingNotificationPlan,
    persisted: bool,
) {
    if !persisted {
        return;
    }
    let Some(message) = plan.message else {
        return;
    };
    let event = tuliprox_core::model::NotificationEvent::from_content(&message);
    // A full or closed outbox hands the event back; fall through to the
    // direct send rather than dropping it outright.
    let event = match crate::recording::recording_supervisor::notification_outbox() {
        Some(outbox) => match outbox.enqueue(event) {
            None => return,
            Some(rejected) => rejected,
        },
        None => event,
    };
    let app_config = Arc::clone(app_config);
    let client = client.clone();
    tokio::spawn(async move {
        tuliprox_messaging::send_event(&app_config, &client, event).await;
    });
}

/// Take the active task out of the candidate, but only when it is still the
/// one this worker is executing.
fn take_active(candidate: &mut PersistedRecordingQueue, uuid: &str) -> Option<PersistedRecordingTask> {
    candidate.active.iter().position(|active| active.uuid == uuid).map(|index| candidate.active.remove(index))
}

async fn requeue_active_download_for_retry(
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

async fn requeue_active_download_for_capacity_wait(
    download_queue: &RecordingQueue,
    uuid: &str,
    reason: &str,
    promote: bool,
    consumed_control: Option<RecordingControl>,
) -> Result<bool, QueueMutationError> {
    let mutation = |candidate: &mut PersistedRecordingQueue| {
        let Some(mut download) = take_active(candidate, uuid) else {
            return Ok(None);
        };
        download.finished = false;
        download.paused = false;
        download.error = Some(reason.to_string());
        download.state = RecordingTaskState::WaitingForCapacity;
        download.next_retry_at = None;
        candidate.queue.insert(0, download);
        if promote {
            crate::recording::recording_queue::promote_from_queue(candidate);
        }
        Ok(Some(true))
    };
    let result = if let Some(control) = consumed_control {
        download_queue.mutate_optional_and_clear_control(uuid, control, mutation).await?
    } else {
        mutate_optional(download_queue, mutation).await?
    };
    Ok(result.unwrap_or(false))
}

/// Write the sidecar for the recording that just finished.
///
/// Best effort by design: the recording is on disk and committed either way,
/// and refusing to complete a transfer because a descriptive file could not be
/// written would trade a real recording for a diagnostic.
async fn write_completion_sidecar(download_queue: &RecordingQueue, uuid: &str, measured_bytes: u64) {
    let Some(task) = download_queue.active.read().await.iter().find(|task| task.uuid == uuid).cloned() else {
        return;
    };
    let Some(relative_path) = task.recording.relative_path.clone() else {
        return;
    };
    let persisted = RecordingQueue::to_persisted(&task);
    let sidecar = recording_sidecar::RecordingSidecar {
        materialization_id: tuliprox_repository::recording_repository::materialization_id_for(&persisted),
        media_identity: persisted.media_identity,
        kind: task.kind,
        relative_path,
        size_bytes: measured_bytes,
        completed_at: chrono::Utc::now().timestamp(),
    };
    if let Err(error) = recording_sidecar::write_sidecar(&task.file_path, &sidecar).await {
        warn!("Could not write the sidecar for {}: {error}", task.file_path.display());
    }
}

const LIVE_CAPACITY_WINDOW_CLOSED: &str = "No provider capacity became available before the recording window closed";

/// Commit a terminal failure for the active task and move on.
async fn fail_active_download(
    download_queue: &RecordingQueue,
    uuid: &str,
    reason: &str,
) -> Result<bool, QueueMutationError> {
    Ok(mutate_optional(download_queue, |candidate| {
        let Some(mut failed) = take_active(candidate, uuid) else {
            return Ok(None);
        };
        failed.finished = true;
        failed.paused = false;
        failed.next_retry_at = None;
        failed.state = RecordingTaskState::Failed;
        failed.error = Some(reason.to_string());
        failed.recording.reserved_bytes = 0;
        candidate.finished.push(failed);
        crate::recording::recording_queue::promote_from_queue(candidate);
        Ok(Some(true))
    })
    .await?
    .unwrap_or(false))
}

async fn promote_ready_downloads(download_queue: &RecordingQueue) -> Result<bool, QueueMutationError> {
    if !download_queue.has_promotable_queued().await {
        return Ok(false);
    }
    Ok(mutate_optional(download_queue, |candidate| {
        let queued = candidate.queue.len();
        while crate::recording::recording_queue::promote_from_queue(candidate).is_some() {}
        Ok((candidate.queue.len() != queued).then_some(true))
    })
    .await?
    .unwrap_or(false))
}

/// Move this worker's task to `finished` and promote the next runnable entry.
///
/// With `waiting_quota`, the task finished a file other entries may be
/// waiting to attach to. Its measured size is checked against each of their
/// quotas before promotion attaches them: a transfer of unknown size cannot
/// be checked when it starts, and attaching charges the whole file.
async fn finish_active_and_promote<F>(
    download_queue: &RecordingQueue,
    uuid: &str,
    waiting_quota: Option<&crate::recording::recording_quota::QuotaLimits>,
    finish: F,
) -> Result<Option<RecordingNotificationPlan>, QueueMutationError>
where
    F: FnOnce(&mut PersistedRecordingTask) -> RecordingNotificationPlan,
{
    download_queue
        .mutate_optional_and_clear_control(uuid, RecordingControl::Cancel, |candidate| {
            let Some(mut active) = take_active(candidate, uuid) else {
                return Ok(None);
            };
            let notification = finish(&mut active);
            let media = active.media_identity.clone();
            let measured = active.recording.measured_bytes;
            candidate.finished.push(active);
            if let Some(limits) = waiting_quota {
                let _ = charge_waiting_siblings(candidate, &media, measured, limits);
            }
            crate::recording::recording_queue::promote_from_queue(candidate);
            Ok(Some(notification))
        })
        .await
}

async fn cancel_active_and_promote(download_queue: &RecordingQueue, uuid: &str) -> Result<bool, QueueMutationError> {
    Ok(download_queue
        .mutate_optional_and_clear_control(uuid, RecordingControl::Cancel, |candidate| {
            let Some(mut active) = take_active(candidate, uuid) else {
                return Ok(None);
            };
            active.finished = true;
            active.paused = false;
            active.next_retry_at = None;
            active.error.get_or_insert_with(|| "Cancelled by user".to_string());
            active.state = RecordingTaskState::Cancelled;
            // Nothing more will be written, so the space stops being spoken for.
            active.recording.reserved_bytes = 0;
            candidate.finished.push(active);
            crate::recording::recording_queue::promote_from_queue(candidate);
            Ok(Some(true))
        })
        .await?
        .unwrap_or(false))
}

enum RetryCommit {
    Waiting { delay_secs: u64, attempts: u8 },
    Failed(RecordingNotificationPlan),
}

async fn prepare_active_retry(
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

const DOWNLOAD_PREEMPTED_REASON: &str = "Preempted by higher-priority foreground stream";
const RECORDING_PREEMPTED_REASON: &str =
    "Recording preempted by higher-priority foreground stream; waiting to resume within the remaining window";

fn preemption_reason_for(download: &RecordingTask) -> &'static str {
    match download.kind {
        RecordingKind::Vod | RecordingKind::Series => DOWNLOAD_PREEMPTED_REASON,
        RecordingKind::Live => RECORDING_PREEMPTED_REASON,
    }
}

pub(crate) const QUOTA_EXCEEDED_DURING_TRANSFER: &str =
    "Quota exceeded: the recording is larger than the quota that is left";
const SIBLING_QUOTA_EXCEEDED: &str = "Quota exceeded: the recording is larger than this entry's quota allows";

/// What the size check at the start of a transfer decided.
struct SizeAdmission {
    /// Byte count the transfer may not exceed when its total is unknown.
    byte_cap: Option<u64>,
    /// Waiting entries for the same media were refused for their own quota.
    siblings_refused: bool,
}

/// Quota and disk admission once a transfer knows how large it is.
///
/// A VOD or series request is admitted before its size is known, so it
/// reserves nothing up front. The first response carries the size, and this
/// is the first point where the charge can be checked: against the
/// transfer's own pool, against every entry waiting to attach to the same
/// file, and against the disk.
struct QuotaGate<'a> {
    queue: &'a RecordingQueue,
    app_config: &'a AppConfig,
}

impl QuotaGate<'_> {
    async fn admit_size(&self, uuid: &str, total: Option<u64>, on_disk: u64) -> Result<SizeAdmission, String> {
        let config = self.app_config.config.load();
        let Some(recording_cfg) = config.recording() else {
            return Ok(SizeAdmission { byte_cap: None, siblings_refused: false });
        };
        let limits = crate::recording::recording_service::quota_limits_from_config(recording_cfg.quota.as_ref());
        // Measured before the mutation: it is a syscall.
        let free = crate::recording::recording_disk::free_bytes_for(Path::new(&recording_cfg.directory));
        let safety = recording_cfg.disk.as_ref().and_then(|disk| disk.safety_bytes).unwrap_or(0);

        let decided = mutate_optional(self.queue, |candidate| {
            let Some(subject) = candidate.active.iter().find(|active| active.uuid == uuid) else {
                return Ok(None);
            };
            let pool = crate::recording::recording_quota::quota_pool_for_task(subject);
            let media = subject.media_identity.clone();
            let used = used_by_others(candidate, uuid, &pool);
            let Some(total) = total else {
                let byte_cap = crate::recording::recording_quota::limit_for_pool(&pool, &limits)
                    .map(|limit| limit.saturating_sub(used));
                return Ok(Some(Ok(SizeAdmission { byte_cap, siblings_refused: false })));
            };
            if over_limit(&pool, used, total, &limits) {
                return Ok(Some(Err(QUOTA_EXCEEDED_DURING_TRANSFER.to_string())));
            }
            if let Some(free) = free {
                let running = crate::recording::recording_disk::active_disk_reservations(
                    all_tasks(candidate).filter(|task| task.uuid != uuid),
                );
                if matches!(
                    crate::recording::recording_disk::would_fit_on_disk(
                        free,
                        safety,
                        running,
                        total.saturating_sub(on_disk)
                    ),
                    crate::recording::recording_disk::DiskAdmission::Insufficient { .. }
                ) {
                    return Ok(Some(Err(DISK_GONE_BEFORE_START.to_string())));
                }
            }
            if let Some(active) = candidate.active.iter_mut().find(|task| task.uuid == uuid) {
                active.recording.reserved_bytes = active.recording.reserved_bytes.max(total);
            }
            let siblings_refused = charge_waiting_siblings(candidate, &media, total, &limits);
            Ok(Some(Ok(SizeAdmission { byte_cap: None, siblings_refused })))
        })
        .await
        .map_err(|err| format!("Could not record the transfer size: {err}"))?;
        // The task left the active slot meanwhile; the worker finds out next.
        decided.unwrap_or(Ok(SizeAdmission { byte_cap: None, siblings_refused: false }))
    }
}

fn all_tasks(candidate: &PersistedRecordingQueue) -> impl Iterator<Item = &PersistedRecordingTask> {
    candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
        .chain(candidate.finished.iter())
}

fn used_by_others(
    candidate: &PersistedRecordingQueue,
    uuid: &str,
    pool: &crate::recording::recording_quota::QuotaPool,
) -> u64 {
    crate::recording::recording_quota::used_bytes_in_pool(all_tasks(candidate).filter(|task| task.uuid != uuid), pool)
}

fn over_limit(
    pool: &crate::recording::recording_quota::QuotaPool,
    used: u64,
    charge: u64,
    limits: &crate::recording::recording_quota::QuotaLimits,
) -> bool {
    matches!(
        crate::recording::recording_quota::would_exceed(pool, used, charge, limits),
        crate::recording::recording_quota::AdmissionOutcome::OverLimit { .. }
    )
}

/// Charge every queued entry that will attach to this file with its size,
/// and refuse the ones whose own quota cannot take it. Returns whether any
/// entry was refused.
///
/// They were admitted while the size was unknown. Attaching later copies the
/// whole file into their charge, so this is the last point where their quota
/// can still say no.
fn charge_waiting_siblings(
    candidate: &mut PersistedRecordingQueue,
    media: &str,
    total: u64,
    limits: &crate::recording::recording_quota::QuotaLimits,
) -> bool {
    if media.is_empty() {
        return false;
    }
    let mut refused = Vec::new();
    let waiting: Vec<String> =
        candidate.queue.iter().filter(|task| task.media_identity == media).map(|task| task.uuid.clone()).collect();
    for uuid in waiting {
        let Some(sibling) = candidate.queue.iter().find(|task| task.uuid == uuid) else {
            continue;
        };
        let pool = crate::recording::recording_quota::quota_pool_for_task(sibling);
        let used = used_by_others(candidate, &uuid, &pool);
        if over_limit(&pool, used, total, limits) {
            refused.push(uuid);
        } else if let Some(sibling) = candidate.queue.iter_mut().find(|task| task.uuid == uuid) {
            sibling.recording.reserved_bytes = sibling.recording.reserved_bytes.max(total);
        }
    }
    for uuid in &refused {
        if let Some(index) = candidate.queue.iter().position(|task| &task.uuid == uuid) {
            let mut failed = candidate.queue.remove(index);
            failed.finished = true;
            failed.paused = false;
            failed.next_retry_at = None;
            failed.state = RecordingTaskState::Failed;
            failed.error = Some(SIBLING_QUOTA_EXCEEDED.to_string());
            failed.recording.reserved_bytes = 0;
            candidate.finished.push(failed);
        }
    }
    !refused.is_empty()
}

pub(crate) const QUOTA_GONE_BEFORE_START: &str = "Quota was exhausted while this recording waited to start";
pub(crate) const DISK_GONE_BEFORE_START: &str = "Disk space was exhausted while this recording waited to start";

/// Re-run admission for the recording that is about to open its
/// destination, returning the reason it can no longer start.
///
/// Admission happened when the request was accepted, which can be a long
/// time before this point: the recording may have waited for provider
/// capacity, sat through a retry backoff, or been scheduled hours ahead.
/// The quota and the free space it was admitted against are not the ones
/// it is about to consume. Without this the first sign of a full disk is
/// a write failure part-way through a recording.
///
/// The subject is excluded from both sums and then added back as the
/// candidate charge, so it is not counted twice.
async fn refused_before_start(
    download_queue: &Arc<RecordingQueue>,
    app_config: &AppConfig,
    uuid: &str,
) -> Option<&'static str> {
    let config = app_config.config.load();
    let recording_cfg = config.recording()?;
    let (_, tasks) = download_queue.committed_snapshot().await;
    let subject = tasks.iter().find(|task| task.uuid == uuid)?;
    let charge = crate::recording::recording_quota::charge_for_task(subject);
    let others = || tasks.iter().filter(|task| task.uuid != uuid);

    let limits = crate::recording::recording_service::quota_limits_from_config(recording_cfg.quota.as_ref());
    let pool = crate::recording::recording_quota::quota_pool_for_task(subject);
    let used = crate::recording::recording_quota::used_bytes_in_pool(others(), &pool);
    if matches!(
        crate::recording::recording_quota::would_exceed(&pool, used, charge, &limits),
        crate::recording::recording_quota::AdmissionOutcome::OverLimit { .. }
    ) {
        return Some(QUOTA_GONE_BEFORE_START);
    }

    // An unmeasurable root is not grounds to refuse, exactly as at admission.
    let free = crate::recording::recording_disk::free_bytes_for(Path::new(&recording_cfg.directory))?;
    let safety = recording_cfg.disk.as_ref().and_then(|disk| disk.safety_bytes).unwrap_or(0);
    let active = crate::recording::recording_disk::active_disk_reservations(others());
    if matches!(
        crate::recording::recording_disk::would_fit_on_disk(free, safety, active, charge),
        crate::recording::recording_disk::DiskAdmission::Insufficient { .. }
    ) {
        return Some(DISK_GONE_BEFORE_START);
    }
    None
}

pub async fn ensure_recording_worker_running<E: EventSink + Clone + 'static>(
    cfg: &AppConfig,
    download_cfg: &RecordingConfig,
    download_queue: &Arc<RecordingQueue>,
    event_manager: &E,
    capacity: &Arc<dyn RecordingCapacityPort>,
    recording_binary: &Path,
) -> Result<(), String> {
    if promote_ready_downloads(download_queue).await.map_err(|err| err.to_string())? {
        publish_recording_change(event_manager);
    }
    let ready: Vec<_> = download_queue
        .active
        .read()
        .await
        .iter()
        .filter(|task| !task.paused && !task.finished)
        .map(|task| task.uuid.clone())
        .collect();
    for uuid in ready {
        start_recording_worker(cfg, download_cfg, download_queue, event_manager, capacity, recording_binary, uuid)
            .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn start_recording_worker<E: EventSink + Clone + 'static>(
    cfg: &AppConfig,
    download_cfg: &RecordingConfig,
    download_queue: &Arc<RecordingQueue>,
    event_manager: &E,
    capacity: &Arc<dyn RecordingCapacityPort>,
    recording_binary: &Path,
    worker_uuid: String,
) -> Result<(), String> {
    let Some(worker) = download_queue.claim_worker(&worker_uuid).await else {
        return Ok(());
    };

    if download_queue.active.read().await.iter().any(|task| task.uuid == worker_uuid) {
        let config = cfg.config.load();
        let disabled_headers = cfg.get_disabled_headers();
        let upstream_headers = request::get_request_headers(
            Some(&download_cfg.headers),
            None,
            disabled_headers.as_ref(),
            config.default_user_agent.as_deref(),
        );
        let dq = Arc::clone(download_queue);
        let control_signal = Arc::clone(&worker.control_signal);
        let control_notify = Arc::clone(&worker.control_notify);
        let event_manager = event_manager.clone();
        let capacity = Arc::clone(capacity);
        let download_cfg = download_cfg.clone();
        let recording_binary = recording_binary.to_path_buf();
        let app_config = Arc::new(cfg.clone());

        // The listener authenticates through its recording URL and bypasses proxies.
        // Provider headers stay on the upstream client; proxied listener requests
        // receive them through the recording input configuration.
        // Notifications retain the proxy without the recording headers.
        let clients = (
            create_client(cfg).no_proxy().redirect(reqwest::redirect::Policy::none()).build(),
            create_client(cfg).redirect(reqwest::redirect::Policy::none()).default_headers(upstream_headers).build(),
            create_client(cfg).build(),
        );
        if let (Ok(transfer_client), Ok(upstream_transfer_client), Ok(client)) = clients {
            if let Some(active) = dq.active.read().await.iter().find(|task| task.uuid == worker_uuid) {
                info!("Starting download worker for active download {} ({})", active.uuid, active.filename);
            }
            tokio::spawn(async move {
                'worker: loop {
                    // One read: the uuid, pause flag and live window must describe
                    // the same task.
                    let active_head = dq
                        .active
                        .read()
                        .await
                        .iter()
                        .find(|task| task.uuid == worker_uuid)
                        .map(|download| (download.paused, recording_deadline_instant(download)));
                    if let Some((paused, window_deadline)) = active_head {
                        if paused {
                            break;
                        }

                        // Acquire a provider connection slot for this download.
                        // If the provider is at capacity, wait in the priority queue until signalled.
                        // Never proceeds without a slot when input_name is set — account bans otherwise.
                        let provider_acquire_result = {
                            let (input_name, priority) =
                                dq.active_scheduling_priority(&worker_uuid).await.unwrap_or((None, 0i8));
                            // Only live work has a window deadline; a transfer waits as long as it takes.
                            if let Some(input_name) = input_name {
                                loop {
                                    if let Some(stopped) = acquire_result_for_control(*control_signal.read().await) {
                                        break stopped;
                                    }
                                    let handle = {
                                        // Keep the policy check and allocation together so
                                        // concurrent recording starts respect background limits.
                                        let _capacity = dq.capacity_guard.lock().await;
                                        let capacities = capacity.capacities_for_input(&input_name).await;
                                        let live_config = app_config.config.load();
                                        let capacity_cfg = live_config.recording().unwrap_or(&download_cfg);
                                        if background_download_should_wait(priority, &capacities, capacity_cfg) {
                                            None
                                        } else {
                                            capacity.acquire(&input_name, priority).await
                                        }
                                    };
                                    if let Some(handle) = handle {
                                        break ProviderAcquireResult::Acquired(Some(handle));
                                    }
                                    if let Err(err) = broadcast_worker_mutation(
                                        &event_manager,
                                        set_active_download_state(
                                            &dq,
                                            &worker_uuid,
                                            RecordingTaskState::WaitingForCapacity,
                                            None,
                                            false,
                                        )
                                        .await,
                                        "waiting-for-capacity state",
                                    ) {
                                        error!("Download worker commit failed: {err}");
                                        break 'worker;
                                    }
                                    // Wait for highest-priority signal — no sleep, no polling.
                                    let waited = wait_for_provider_slot(
                                        &dq,
                                        &input_name,
                                        priority,
                                        control_signal.as_ref(),
                                        control_notify.as_ref(),
                                        window_deadline,
                                    )
                                    .await;
                                    if let Some(stopped) = acquire_result_after_wait(waited) {
                                        break stopped;
                                    }
                                }
                            } else {
                                ProviderAcquireResult::Acquired(None)
                            }
                        };

                        let provider_handle = match provider_acquire_result {
                            ProviderAcquireResult::Acquired(handle) => {
                                match commit_acquired_download(&dq, &worker_uuid).await {
                                    Ok(Some(notification)) => {
                                        spawn_recording_notification_after_persist(
                                            &app_config,
                                            &client,
                                            notification,
                                            true,
                                        );
                                        publish_recording_change(&event_manager);
                                    }
                                    Ok(None) => {
                                        capacity.release(handle).await;
                                        error!("Download worker active task changed after provider acquire");
                                        break 'worker;
                                    }
                                    Err(err) => {
                                        capacity.release(handle).await;
                                        error!("Download worker commit failed after provider acquire: {err}");
                                        break 'worker;
                                    }
                                }
                                handle
                            }
                            ProviderAcquireResult::WindowClosed => {
                                // No slot was ever acquired, so there is nothing
                                // to release and nothing was written.
                                if let Err(err) = broadcast_required_worker_mutation(
                                    &event_manager,
                                    fail_active_download(&dq, &worker_uuid, LIVE_CAPACITY_WINDOW_CLOSED).await,
                                    "live capacity window closed",
                                ) {
                                    error!("Download worker commit failed: {err}");
                                    break 'worker;
                                }
                                continue;
                            }
                            ProviderAcquireResult::Paused => {
                                break;
                            }
                            ProviderAcquireResult::Cancelled => {
                                if let Err(err) = broadcast_required_worker_mutation(
                                    &event_manager,
                                    cancel_active_and_promote(&dq, &worker_uuid).await,
                                    "provider-wait cancellation",
                                ) {
                                    error!("Download worker commit failed: {err}");
                                    break 'worker;
                                }
                                continue;
                            }
                            ProviderAcquireResult::Preempted => {
                                if let Err(err) = broadcast_required_worker_mutation(
                                    &event_manager,
                                    requeue_active_download_for_capacity_wait(
                                        &dq,
                                        &worker_uuid,
                                        "Reloading download service configuration",
                                        true,
                                        Some(RecordingControl::Restart),
                                    )
                                    .await,
                                    "configuration-reload requeue",
                                ) {
                                    error!("Download worker commit failed: {err}");
                                    break 'worker;
                                }
                                break;
                            }
                        };

                        let mut execution_result = {
                            let Some(download) = active_download_snapshot_for_worker(&dq.active, &worker_uuid).await
                            else {
                                capacity.release(provider_handle).await;
                                break 'worker;
                            };
                            // Last point before a destination is opened.
                            if let Some(reason) = refused_before_start(&dq, &app_config, &worker_uuid).await {
                                capacity.release(provider_handle).await;
                                match fail_active_download(&dq, &worker_uuid, reason).await {
                                    Ok(_) => publish_recording_change(&event_manager),
                                    Err(err) => {
                                        error!("Download worker commit failed: {err}");
                                        break 'worker;
                                    }
                                }
                                continue;
                            }
                            match download.kind {
                                RecordingKind::Vod | RecordingKind::Series => 'http_execution: {
                                    let execution_download = match recording_execution_download(
                                        &app_config,
                                        &download,
                                        provider_handle.as_ref(),
                                    ) {
                                        Ok(execution) => execution,
                                        Err(error) => break 'http_execution DownloadExecutionResult::Failed(error),
                                    };
                                    download_file(
                                        Arc::clone(&dq.active),
                                        execution_download,
                                        &transfer_client,
                                        Some(&upstream_transfer_client),
                                        Arc::clone(&control_signal),
                                        Arc::clone(&control_notify),
                                        Some(&event_manager),
                                        Some(QuotaGate { queue: &dq, app_config: &app_config }),
                                    )
                                    .await
                                }
                                RecordingKind::Live => 'recording_execution: {
                                    let execution_download = match recording_execution_download(
                                        &app_config,
                                        &download,
                                        provider_handle.as_ref(),
                                    ) {
                                        Ok(execution_download) => execution_download,
                                        Err(err) => break 'recording_execution DownloadExecutionResult::Failed(err),
                                    };
                                    let progress_path = recording_partial_path(&execution_download.file_path);
                                    let container_format =
                                        app_config.config.load().recording().map_or_else(
                                            shared::model::RecordingContainerFormat::default,
                                            |recording| recording.container_format,
                                        );
                                    let mut recording_future = Box::pin(run_recording_with_binary(
                                        &recording_binary,
                                        &execution_download,
                                        &control_signal,
                                        &control_notify,
                                        None,
                                        container_format,
                                    ));
                                    let mut progress_tick = time::interval(RECORDING_PROGRESS_UPDATE_INTERVAL);
                                    progress_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

                                    let result = loop {
                                        tokio::select! {
                                            recording_result = &mut recording_future => break recording_result,
                                            _ = progress_tick.tick() => {
                                                refresh_recording_progress(
                                                    &dq.active,
                                                    &worker_uuid,
                                                    &progress_path,
                                                    &event_manager,
                                                )
                                                .await;
                                            }
                                        }
                                    };
                                    let final_progress_path = if matches!(&result, RecordingExecutionResult::Completed)
                                    {
                                        &execution_download.file_path
                                    } else {
                                        &progress_path
                                    };
                                    refresh_recording_progress(
                                        &dq.active,
                                        &worker_uuid,
                                        final_progress_path,
                                        &event_manager,
                                    )
                                    .await;

                                    match result {
                                        RecordingExecutionResult::Completed => DownloadExecutionResult::Completed,
                                        RecordingExecutionResult::Paused => DownloadExecutionResult::Paused,
                                        RecordingExecutionResult::Cancelled => DownloadExecutionResult::Cancelled,
                                        RecordingExecutionResult::Preempted => DownloadExecutionResult::Preempted,
                                        RecordingExecutionResult::Retryable(err) => {
                                            DownloadExecutionResult::Retryable(err)
                                        }
                                        RecordingExecutionResult::Failed(err) => DownloadExecutionResult::Failed(err),
                                    }
                                }
                            }
                        };

                        // The internal HTTP stream owns the same provider allocation, and its
                        // normal EOF/client-close cleanup cancels the allocation token too, so
                        // cancellation alone does not mean foreground preemption. The manager
                        // records an explicit close reason before a real priority eviction;
                        // only that outcome requeues the recording.
                        if provider_handle.as_ref().is_some_and(|handle| {
                            handle.get_close_reason() == tuliprox_core::model::ProviderCloseReason::PriorityPreempted
                        }) {
                            execution_result = DownloadExecutionResult::Preempted;
                        }

                        match execution_result {
                            DownloadExecutionResult::Completed => {
                                capacity.release(provider_handle).await;
                                // Path copied out first: the lock must not be held across
                                // the filesystem call, or progress writers and commits stall.
                                let finished_file = dq
                                    .active
                                    .read()
                                    .await
                                    .iter()
                                    .find(|task| task.uuid == worker_uuid)
                                    .map(|fd| (fd.file_path.clone(), fd.size));
                                let measured_bytes = match finished_file {
                                    Some((path, fallback)) => {
                                        tokio::fs::metadata(&path).await.map_or(fallback, |metadata| metadata.len())
                                    }
                                    None => 0,
                                };
                                // Beside the file, before the repository is told
                                // it is complete: an operator finding an orphan
                                // needs the record even if the commit is what
                                // failed.
                                write_completion_sidecar(&dq, &worker_uuid, measured_bytes).await;
                                let completion_limits = app_config.config.load().recording().map(|recording| {
                                    crate::recording::recording_service::quota_limits_from_config(
                                        recording.quota.as_ref(),
                                    )
                                });
                                let committed =
                                    finish_active_and_promote(&dq, &worker_uuid, completion_limits.as_ref(), |fd| {
                                        fd.finished = true;
                                        fd.paused = false;
                                        fd.state = RecordingTaskState::Completed;
                                        fd.size = measured_bytes;
                                        fd.error = None;
                                        fd.next_retry_at = None;
                                        let meta = &mut fd.recording;
                                        meta.measured_bytes = measured_bytes;
                                        meta.reserved_bytes = 0;
                                        meta.completed_at = Some(chrono::Utc::now().timestamp());
                                        meta.partial_relative_path = None;
                                        mark_recording_metadata_notification(
                                            &mut fd.recording,
                                            LifecycleEvent::Completed,
                                            None,
                                        )
                                    })
                                    .await;
                                match committed {
                                    Ok(Some(notification)) => {
                                        spawn_recording_notification_after_persist(
                                            &app_config,
                                            &client,
                                            notification,
                                            true,
                                        );
                                        publish_recording_change(&event_manager);
                                    }
                                    Ok(None) => {}
                                    Err(err) => {
                                        error!("Failed to persist completed download state: {err}");
                                        break 'worker;
                                    }
                                }
                            }
                            DownloadExecutionResult::Paused => {
                                capacity.release(provider_handle).await;
                                // A resume observed here continues this worker. A later resume
                                // is picked up after release_worker clears the claim and wakes
                                // the scheduler to start a new worker.
                                if continue_after_pause(&dq, &worker_uuid).await {
                                    continue 'worker;
                                }
                                break 'worker;
                            }
                            DownloadExecutionResult::Cancelled => {
                                capacity.release(provider_handle).await;
                                if let Err(err) = broadcast_required_worker_mutation(
                                    &event_manager,
                                    cancel_active_and_promote(&dq, &worker_uuid).await,
                                    "cancelled state",
                                ) {
                                    error!("Download worker commit failed: {err}");
                                    break 'worker;
                                }
                            }
                            DownloadExecutionResult::Preempted => {
                                capacity.release(provider_handle).await;
                                let control = *control_signal.read().await;
                                match control {
                                    RecordingControl::Restart => warn!(
                                        "Active transfer is restarting to apply updated download service configuration"
                                    ),
                                    _ => warn!("Active transfer was preempted by a higher-priority stream"),
                                }
                                let reason = {
                                    let active = dq.active.read().await;
                                    if control == RecordingControl::Restart {
                                        "Reloading download service configuration"
                                    } else {
                                        active
                                            .iter()
                                            .find(|task| task.uuid == worker_uuid)
                                            .map_or(DOWNLOAD_PREEMPTED_REASON, preemption_reason_for)
                                    }
                                };
                                if let Err(err) = broadcast_required_worker_mutation(
                                    &event_manager,
                                    requeue_active_download_for_capacity_wait(
                                        &dq,
                                        &worker_uuid,
                                        reason,
                                        true,
                                        (control == RecordingControl::Restart).then_some(RecordingControl::Restart),
                                    )
                                    .await,
                                    "preempted requeue",
                                ) {
                                    error!("Download worker commit failed: {err}");
                                    break 'worker;
                                }
                                if should_exit_worker_after_preempt(control) {
                                    break;
                                }
                            }
                            DownloadExecutionResult::Retryable(err) => {
                                capacity.release(provider_handle).await;
                                let err = redact_url_tokens(&err);
                                warn!("Recording transfer encountered a transient failure: {err}");
                                let retry_commit = prepare_active_retry(&dq, &worker_uuid, &download_cfg, &err).await;
                                let retry_delay_secs = match retry_commit {
                                    Ok(Some(RetryCommit::Waiting { delay_secs, attempts })) => {
                                        debug!("Download retry attempt {attempts} scheduled in {delay_secs}s");
                                        publish_recording_change(&event_manager);
                                        delay_secs
                                    }
                                    Ok(Some(RetryCommit::Failed(notification))) => {
                                        spawn_recording_notification_after_persist(
                                            &app_config,
                                            &client,
                                            notification,
                                            true,
                                        );
                                        publish_recording_change(&event_manager);
                                        if dq.active.read().await.iter().any(|task| task.uuid == worker_uuid) {
                                            continue;
                                        }
                                        break;
                                    }
                                    Ok(None) => break,
                                    Err(err) => {
                                        error!("Failed to persist retry state: {err}");
                                        break;
                                    }
                                };
                                let mut retry_sleep = Box::pin(time::sleep(Duration::from_secs(retry_delay_secs)));
                                let retry_wait_outcome = loop {
                                    tokio::select! {
                                        () = &mut retry_sleep => break DownloadExecutionResult::Retryable(String::new()),
                                        () = control_notify.notified() => {
                                            match *control_signal.read().await {
                                                RecordingControl::Pause => break DownloadExecutionResult::Paused,
                                                RecordingControl::Cancel => break DownloadExecutionResult::Cancelled,
                                                RecordingControl::Restart => break DownloadExecutionResult::Preempted,
                                                RecordingControl::None => {}
                                            }
                                        }
                                    }
                                };

                                match retry_wait_outcome {
                                    DownloadExecutionResult::Retryable(_) => {
                                        if let Err(err) = broadcast_required_worker_mutation(
                                            &event_manager,
                                            requeue_active_download_for_retry(&dq, &worker_uuid, true).await,
                                            "retry requeue",
                                        ) {
                                            error!("Download worker commit failed: {err}");
                                            break 'worker;
                                        }
                                    }
                                    DownloadExecutionResult::Paused => {
                                        if let Err(err) = broadcast_required_worker_mutation(
                                            &event_manager,
                                            set_active_download_state(
                                                &dq,
                                                &worker_uuid,
                                                RecordingTaskState::Paused,
                                                None,
                                                true,
                                            )
                                            .await,
                                            "paused retry state",
                                        ) {
                                            error!("Download worker commit failed: {err}");
                                            break 'worker;
                                        }
                                        break;
                                    }
                                    DownloadExecutionResult::Cancelled => {
                                        if let Err(err) = broadcast_required_worker_mutation(
                                            &event_manager,
                                            cancel_active_and_promote(&dq, &worker_uuid).await,
                                            "cancelled retry state",
                                        ) {
                                            error!("Download worker commit failed: {err}");
                                            break 'worker;
                                        }
                                    }
                                    DownloadExecutionResult::Completed | DownloadExecutionResult::Failed(_) => {}
                                    DownloadExecutionResult::Preempted => {
                                        if let Err(err) = broadcast_required_worker_mutation(
                                            &event_manager,
                                            requeue_active_download_for_capacity_wait(
                                                &dq,
                                                &worker_uuid,
                                                "Reloading download service configuration",
                                                true,
                                                Some(RecordingControl::Restart),
                                            )
                                            .await,
                                            "configuration-reload retry requeue",
                                        ) {
                                            error!("Download worker commit failed: {err}");
                                            break 'worker;
                                        }
                                        break;
                                    }
                                }
                            }
                            DownloadExecutionResult::Failed(err) => {
                                capacity.release(provider_handle).await;
                                let err = redact_url_tokens(&err);
                                warn!("Download failed permanently: {err}");
                                let committed = finish_active_and_promote(&dq, &worker_uuid, None, |fd| {
                                    fd.finished = true;
                                    fd.paused = false;
                                    fd.next_retry_at = None;
                                    fd.error = Some(err.clone());
                                    fd.state = RecordingTaskState::Failed;
                                    fd.recording.reserved_bytes = 0;
                                    mark_recording_metadata_notification(
                                        &mut fd.recording,
                                        LifecycleEvent::Failed,
                                        Some(err),
                                    )
                                })
                                .await;
                                match committed {
                                    Ok(Some(notification)) => {
                                        spawn_recording_notification_after_persist(
                                            &app_config,
                                            &client,
                                            notification,
                                            true,
                                        );
                                        publish_recording_change(&event_manager);
                                    }
                                    Ok(None) => {}
                                    Err(commit_err) => {
                                        error!("Failed to persist failed download state: {commit_err}");
                                        break 'worker;
                                    }
                                }
                            }
                        }
                    } else {
                        break;
                    }
                }
                dq.release_worker(&worker_uuid).await;
            });
        } else {
            *worker.running.write().await = false;
            return Err("Failed to build http client".to_string());
        }
    } else {
        download_queue.release_worker(&worker_uuid).await;
    }
    Ok(())
}

pub fn spawn_recording_services<E: EventSink + Clone + 'static>(
    ctx: &RecordingCtx<E>,
    cancel_token: &CancellationToken,
) {
    let config = ctx.app_config.config.load();
    let Some(recording_cfg) = config.recording().cloned() else {
        return;
    };

    start_recording_scheduler(
        Arc::clone(&ctx.app_config),
        recording_cfg,
        &ctx.recordings,
        ctx.events.clone(),
        Arc::clone(&ctx.recording_capacity),
        cancel_token.clone(),
        Path::new(crate::recording::recording_worker::FFMPEG_BINARY).to_path_buf(),
    );
}

pub async fn resume_recording_worker_if_needed<E: EventSink + Clone + 'static>(
    ctx: &RecordingCtx<E>,
    recording_cfg: &RecordingConfig,
) -> Result<(), String> {
    if ctx.recordings.queue.lock().await.is_empty() && ctx.recordings.active.read().await.is_empty() {
        return Ok(());
    }

    ensure_recording_worker_running(
        &ctx.app_config,
        recording_cfg,
        &ctx.recordings,
        &ctx.events,
        &ctx.recording_capacity,
        Path::new(crate::recording::recording_worker::FFMPEG_BINARY),
    )
    .await
}

fn start_recording_scheduler<E: EventSink + Clone + 'static>(
    app_config: Arc<AppConfig>,
    recording_cfg: RecordingConfig,
    recordings: &Arc<RecordingQueue>,
    event_manager: E,
    capacity: Arc<dyn RecordingCapacityPort>,
    cancel_token: CancellationToken,
    recording_binary: std::path::PathBuf,
) {
    let capacity_notify = capacity.capacity_changed();
    let slot_waiters = Arc::clone(&recordings.slot_waiters);
    let bridge_capacity = Arc::clone(&capacity);
    let bridge_recording_cfg = recording_cfg.clone();
    let bridge_app_config = Arc::clone(&app_config);
    let bridge_cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = bridge_cancel_token.cancelled() => break,
                () = capacity_notify.notified() => {},
                () = slot_waiters.registration_changed.notified() => {}
            }
            let mut capacities_by_input: HashMap<Arc<str>, ProviderCapacities> = HashMap::new();
            let live_config = bridge_app_config.config.load();
            let waiter_cfg = live_config.recording().unwrap_or(&bridge_recording_cfg);
            let mut waiters = slot_waiters.snapshots();
            waiters.sort_by_key(|waiter| waiter.priority);
            for waiter in waiters {
                let Some(input_name) = waiter.input_name.as_ref() else {
                    let _ = slot_waiters.signal_waiter(waiter.id);
                    continue;
                };
                let capacities = match capacities_by_input.entry(Arc::clone(input_name)) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(bridge_capacity.capacities_for_input(input_name).await)
                    }
                };
                if !capacities_have_free_slot(capacities)
                    || background_download_should_wait(waiter.priority, capacities, waiter_cfg)
                {
                    continue;
                }
                if slot_waiters.signal_waiter(waiter.id) {
                    // Reserve a wake-up against this snapshot so one signal wakes
                    // as many eligible recordings as there are free connections.
                    if let Some((_, used, _)) =
                        capacities.iter_mut().find(|(_, used, limit)| *limit == 0 || *used < *limit)
                    {
                        *used = used.saturating_add(1);
                    }
                }
            }
        }
    });

    let scheduler_recordings = Arc::clone(recordings);
    let scheduler_cancel_token = cancel_token;
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = scheduler_cancel_token.cancelled() => break,
                _ = interval.tick() => {},
                () = scheduler_recordings.queue_changed.notified() => {}
            }
            let promoted = scheduler_recordings.promote_due_scheduled_now().await;
            if promoted > 0 {
                publish_recording_change(&event_manager);
            }
            // Commits and worker release wake the scheduler immediately. The
            // interval also checks scheduled windows and retries failed starts.
            if let Err(error) = ensure_recording_worker_running(
                &app_config,
                &recording_cfg,
                &scheduler_recordings,
                &event_manager,
                &capacity,
                &recording_binary,
            )
            .await
            {
                error!("Could not start recording workers: {error}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_result_after_wait, acquire_result_for_control, continue_after_pause, download_file,
        ensure_recording_worker_running, finalize_http_transfer, finish_active_and_promote, http_transfer_path,
        recording_deadline_instant, refresh_recording_progress, requeue_active_download_for_capacity_wait,
        start_recording_scheduler, wait_for_provider_slot, DownloadExecutionResult, ProviderAcquireResult, QuotaGate,
        RecordingNotificationPlan, DISK_GONE_BEFORE_START, DOWNLOAD_PREEMPTED_REASON, LIVE_CAPACITY_WINDOW_CLOSED,
        QUOTA_EXCEEDED_DURING_TRANSFER,
    };
    use crate::recording::{
        recording_capacity::{stub::StubCapacity, RecordingCapacityPort},
        recording_queue::{
            PersistedRecordingTask, RecordingControl, RecordingPartition, RecordingQueue, RecordingTask,
            RecordingWaitOutcome,
        },
    };
    use shared::model::{
        EventMessage, EventSink, NoopSink, RecordingKind, RecordingMetadata, RecordingOwner, RecordingSource,
        RecordingTaskState, RecordingVisibility, UserId,
    };
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tempfile::TempDir;
    use tokio::sync::{Notify, RwLock};
    use tuliprox_core::model::RecordingConfig;

    #[derive(Clone, Default)]
    struct ProgressSink(Arc<AtomicUsize>);

    impl EventSink for ProgressSink {
        fn emit(&self, event: EventMessage) {
            if matches!(event, EventMessage::RecordingProgress) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A task whose Live window runs from `program_start` for `duration_secs`.
    fn scheduled_task(kind: RecordingKind, program_start: i64, duration_secs: i64) -> RecordingTask {
        let meta = RecordingMetadata::new_live(
            RecordingOwner::User(UserId::from("web:alice")),
            RecordingVisibility::Private,
            RecordingSource::new("t1", "v1", "in1"),
            program_start,
            program_start + duration_secs,
            0,
            0,
        );
        RecordingQueue::from_persisted(PersistedRecordingTask {
            media_identity: String::new(),
            partition: RecordingPartition::default(),
            uuid: "task".to_string(),
            kind,
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/capture.ts"),
            filename: "capture.ts".to_string(),
            url: "https://example.com/stream".to_string(),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: RecordingTaskState::Running,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: meta,
        })
        .expect("valid fixture")
    }

    fn serve_range_fixture(ignore_range: bool) -> (reqwest::Url, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let url = reqwest::Url::parse(&format!("http://{}/download", listener.local_addr().expect("server address")))
            .expect("fixture URL");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept download request");
            socket.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).expect("read download request");
                assert!(read > 0, "request ended before headers");
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8(request).expect("request headers");
            let (status, headers, body) = if ignore_range {
                ("200 OK", "", b"0123456789".as_slice())
            } else {
                ("206 Partial Content", "Content-Range: bytes 4-9/10\r\n", b"456789".as_slice())
            };
            let response =
                format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            socket.write_all(response.as_bytes()).expect("write response headers");
            socket.write_all(body).expect("write response body");
            request
        });
        (url, server)
    }

    #[tokio::test]
    async fn vod_and_series_resume_at_the_saved_byte_offset() {
        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            let dir = TempDir::new().expect("recording directory");
            let (url, server) = serve_range_fixture(false);
            let mut task = scheduled_task(kind, chrono::Utc::now().timestamp(), 900);
            task.file_dir = dir.path().to_path_buf();
            task.file_path = dir.path().join("recording.mp4");
            task.url = url;
            task.total_size = Some(10);
            let partial = http_transfer_path(&task);
            tokio::fs::write(&partial, b"0123").await.expect("saved partial");
            let active = Arc::new(RwLock::new(vec![task.clone()]));
            let result = download_file::<NoopSink>(
                active,
                task.clone(),
                &reqwest::Client::new(),
                None,
                Arc::new(RwLock::new(RecordingControl::None)),
                Arc::new(Notify::new()),
                None,
                None,
            )
            .await;
            assert!(matches!(result, DownloadExecutionResult::Completed));
            assert_eq!(tokio::fs::read(&task.file_path).await.expect("completed recording"), b"0123456789");
            assert!(!partial.exists());
            let request = server.join().expect("fixture server").to_ascii_lowercase();
            assert!(request.contains("range: bytes=4-\r\n"), "missing resume Range: {request}");
            assert!(request.contains("accept-encoding: identity\r\n"), "missing identity encoding: {request}");
        }
    }

    #[tokio::test]
    async fn resume_during_pause_shutdown_keeps_a_worker_for_vod_and_series() {
        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            let queue = RecordingQueue::new();
            *queue.active.write().await = vec![scheduled_task(kind, 0, 900)];
            *queue.worker("task").running.write().await = true;

            assert!(queue.pause_active("task").await.expect("pause"));
            assert!(queue.resume_active("task").await.expect("resume before old worker exits"));
            assert!(continue_after_pause(&queue, "task").await);
            assert!(queue.workers_running().await);
            assert_eq!(queue.active.read().await.first().map(|task| task.state), Some(RecordingTaskState::Running));

            assert!(queue.pause_active("task").await.expect("second pause"));
            assert!(!continue_after_pause(&queue, "task").await);
            assert!(queue.workers_running().await, "the claim lasts until worker exit");
            queue.release_worker("task").await;
            assert!(!queue.workers_running().await);
            assert!(queue.resume_active("task").await.expect("resume after old worker exits"));
            assert!(!queue.active.read().await.first().is_some_and(|task| task.paused));
        }
    }

    #[tokio::test]
    async fn ignored_range_preserves_vod_and_series_partials() {
        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            let dir = TempDir::new().expect("recording directory");
            let (url, server) = serve_range_fixture(true);
            let mut task = scheduled_task(kind, chrono::Utc::now().timestamp(), 900);
            task.file_dir = dir.path().to_path_buf();
            task.file_path = dir.path().join("recording.mp4");
            task.url = url;
            task.total_size = Some(10);
            let partial = http_transfer_path(&task);
            tokio::fs::write(&partial, b"0123").await.expect("saved partial");
            let result = download_file::<NoopSink>(
                Arc::new(RwLock::new(vec![task.clone()])),
                task.clone(),
                &reqwest::Client::new(),
                None,
                Arc::new(RwLock::new(RecordingControl::None)),
                Arc::new(Notify::new()),
                None,
                None,
            )
            .await;
            assert!(
                matches!(result, DownloadExecutionResult::Failed(error) if error == super::RANGE_UNSUPPORTED_ERROR)
            );
            assert_eq!(tokio::fs::read(&partial).await.expect("partial preserved"), b"0123");
            assert!(!task.file_path.exists());
            assert!(server.join().expect("fixture server").to_ascii_lowercase().contains("range: bytes=4-\r\n"));
        }
    }

    #[tokio::test]
    async fn live_byte_progress_is_visible_without_a_queue_revision_change() {
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("capture.ts.partial");
        std::fs::write(&partial, b"growing capture").expect("write partial recording");
        let queue = RecordingQueue::new();
        *queue.active.write().await = vec![scheduled_task(RecordingKind::Live, 0, 60)];
        let events = ProgressSink::default();

        refresh_recording_progress(&queue.active, "task", &partial, &events).await;
        let (revision, tasks) = queue.committed_snapshot().await;

        assert_eq!(revision.0, 0);
        assert_eq!(tasks[0].size, b"growing capture".len() as u64);
        assert_eq!(events.0.load(Ordering::Relaxed), 1);

        refresh_recording_progress(&queue.active, "task", &partial, &events).await;
        assert_eq!(events.0.load(Ordering::Relaxed), 1, "unchanged bytes need no second event");
    }

    #[tokio::test]
    async fn a_restart_requeue_consumes_the_restart_signal() {
        // A restart that survives its own requeue makes every new worker
        // preempt itself on start, spinning until the live window closes.
        let queue = RecordingQueue::new();
        *queue.active.write().await = vec![scheduled_task(RecordingKind::Live, 0, 60)];
        *queue.worker("task").control_signal.write().await = RecordingControl::Restart;

        let requeued = requeue_active_download_for_capacity_wait(
            &queue,
            "task",
            "Reloading download service configuration",
            false,
            Some(RecordingControl::Restart),
        )
        .await
        .expect("requeue");

        assert!(requeued);
        assert_eq!(*queue.worker("task").control_signal.read().await, RecordingControl::None);
    }

    #[tokio::test]
    async fn a_preemption_requeue_leaves_a_pending_control_untouched() {
        let queue = RecordingQueue::new();
        *queue.active.write().await = vec![scheduled_task(RecordingKind::Live, 0, 60)];
        *queue.worker("task").control_signal.write().await = RecordingControl::Pause;

        requeue_active_download_for_capacity_wait(&queue, "task", DOWNLOAD_PREEMPTED_REASON, false, None)
            .await
            .expect("requeue");

        assert_eq!(*queue.worker("task").control_signal.read().await, RecordingControl::Pause);
    }

    /// One VOD on one media for `owner`, in the given partition and state.
    fn vod_entry(uuid: &str, owner: &str, state: RecordingTaskState) -> PersistedRecordingTask {
        let mut task = scheduled_task(RecordingKind::Vod, 0, 0);
        task.uuid = uuid.to_string();
        task.state = state;
        task.recording = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::from(owner)),
            RecordingVisibility::Private,
            RecordingSource::new("t1", "v1", "in1"),
            "Film".to_string(),
        );
        let mut persisted = RecordingQueue::to_persisted(&task);
        persisted.media_identity = "film".to_string();
        persisted
    }

    #[tokio::test]
    async fn starting_workers_commits_entries_attached_to_an_already_completed_file(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let queue = Arc::new(RecordingQueue::new());
        let mut done = vod_entry("done", "alice", RecordingTaskState::Completed);
        done.finished = true;
        done.size = 123;
        done.recording.measured_bytes = 123;
        let waiting = vod_entry("waiting", "bob", RecordingTaskState::Queued);
        crate::recording::recording_queue::mutate(&queue, |candidate| {
            candidate.finished.push(done);
            candidate.queue.push(waiting);
            Ok(())
        })
        .await?;
        let stub = StubCapacity::with_room();
        let capacity: Arc<dyn RecordingCapacityPort> = stub.clone();
        ensure_recording_worker_running(
            &app_config_with_listener(),
            &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
            &queue,
            &NoopSink,
            &capacity,
            Path::new("unused-encoder"),
        )
        .await?;
        assert!(queue.queue.lock().await.is_empty());
        assert!(queue.active.read().await.is_empty());
        assert_eq!(queue.finished.read().await.len(), 2);
        let finished = queue.finished.read().await;
        let attached = finished.iter().find(|task| task.uuid == "waiting").ok_or("missing attached entry")?;
        assert_eq!(attached.state, RecordingTaskState::Completed);
        assert_eq!(attached.size, 123);
        assert_eq!(stub.acquire_count(), 0);
        Ok(())
    }

    /// Recording enabled under `dir` with a private quota of `quota` bytes.
    fn app_config_with_quota(dir: &Path, quota: u64) -> tuliprox_core::model::AppConfig {
        let app_config = bare_app_config();
        let mut rec_cfg =
            RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() });
        rec_cfg.directory = dir.to_string_lossy().into_owned();
        rec_cfg.quota = Some(tuliprox_core::model::RecordingQuotaConfig {
            default_private_bytes: Some(quota),
            ..Default::default()
        });
        app_config.config.store(Arc::new(tuliprox_core::model::Config {
            video: Some(tuliprox_core::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(rec_cfg),
            }),
            ..tuliprox_core::model::Config::default()
        }));
        app_config
    }

    async fn queue_with(
        dir: &TempDir,
        active: PersistedRecordingTask,
        queued: Vec<PersistedRecordingTask>,
    ) -> RecordingQueue {
        let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository");
        crate::recording::recording_queue::mutate(&queue, move |candidate| {
            candidate.active = vec![active.clone()];
            candidate.queue.clone_from(&queued);
            Ok(())
        })
        .await
        .expect("seed");
        queue
    }

    #[tokio::test]
    async fn a_transfer_larger_than_the_quota_left_is_refused_once_its_size_is_known() {
        // Admitted at zero because the size was unknown; the first response
        // says 4096 bytes against a 16 byte quota.
        let dir = TempDir::new().expect("tempdir");
        let queue = queue_with(&dir, vod_entry("alice", "web:alice", RecordingTaskState::Running), Vec::new()).await;
        let app_config = app_config_with_quota(dir.path(), 16);
        let gate = QuotaGate { queue: &queue, app_config: &app_config };

        let refused = gate.admit_size("alice", Some(4096), 0).await.err();
        assert_eq!(refused.as_deref(), Some(QUOTA_EXCEEDED_DURING_TRANSFER));

        let admitted = gate.admit_size("alice", Some(16), 0).await.expect("fits");
        assert_eq!(admitted.byte_cap, None);
        let reserved = queue.active.read().await.first().map(|active| active.recording.reserved_bytes);
        assert_eq!(reserved, Some(16), "the known size is now reserved");
    }

    #[tokio::test]
    async fn a_download_over_the_quota_stops_before_writing_its_file() {
        let dir = TempDir::new().expect("tempdir");
        let (url, server) = serve_range_fixture(true);
        let mut task = scheduled_task(RecordingKind::Vod, chrono::Utc::now().timestamp(), 900);
        task.file_dir = dir.path().to_path_buf();
        task.file_path = dir.path().join("film.mp4");
        task.url = url;
        task.recording = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::from("web:alice")),
            RecordingVisibility::Private,
            RecordingSource::new("t1", "v1", "in1"),
            "Film".to_string(),
        );
        let queue = queue_with(&dir, RecordingQueue::to_persisted(&task), Vec::new()).await;
        let app_config = app_config_with_quota(dir.path(), 4);

        let result = download_file::<NoopSink>(
            Arc::clone(&queue.active),
            task.clone(),
            &reqwest::Client::new(),
            None,
            Arc::new(RwLock::new(RecordingControl::None)),
            Arc::new(Notify::new()),
            None,
            Some(QuotaGate { queue: &queue, app_config: &app_config }),
        )
        .await;
        let _ = server.join();

        assert!(
            matches!(&result, DownloadExecutionResult::Failed(reason) if reason == QUOTA_EXCEEDED_DURING_TRANSFER),
            "a 10 byte file against a 4 byte quota"
        );
        assert!(!task.file_path.exists(), "nothing was published");
    }

    #[tokio::test]
    async fn a_file_of_unknown_size_is_checked_against_waiting_quotas_when_it_completes() {
        // Without a Content-Length nothing can be charged when the transfer
        // starts. Alice has room; Bob, waiting to attach, has 16 bytes. The
        // finished size is the first moment his quota can be asked.
        let dir = TempDir::new().expect("tempdir");
        let queue = queue_with(
            &dir,
            vod_entry("alice", "web:alice", RecordingTaskState::Running),
            vec![vod_entry("bob", "web:bob", RecordingTaskState::Queued)],
        )
        .await;
        let app_config = app_config_with_quota(dir.path(), 1 << 20);
        let gate = QuotaGate { queue: &queue, app_config: &app_config };
        let started = gate.admit_size("alice", None, 0).await.expect("alice starts");
        assert!(!started.siblings_refused, "nothing is known about the size yet");

        let limits = crate::recording::recording_quota::QuotaLimits {
            default_private_bytes: Some(16),
            per_user_bytes: [(UserId::from("web:alice"), 1 << 20)].into_iter().collect(),
            shared_bytes: None,
        };
        finish_active_and_promote(&queue, "alice", Some(&limits), |done| {
            done.finished = true;
            done.state = RecordingTaskState::Completed;
            done.size = 4096;
            done.recording.measured_bytes = 4096;
            done.recording.reserved_bytes = 0;
            RecordingNotificationPlan::empty()
        })
        .await
        .expect("commit");

        let finished = queue.finished.read().await.clone();
        let state_of = |uuid: &str| finished.iter().find(|task| task.uuid == uuid).map(|task| task.state);
        assert_eq!(state_of("alice"), Some(RecordingTaskState::Completed));
        assert_eq!(state_of("bob"), Some(RecordingTaskState::Failed), "not attached past his quota");
        assert!(queue.queue.lock().await.is_empty());
    }

    #[tokio::test]
    async fn a_transfer_of_unknown_size_is_capped_at_the_quota_left() {
        let dir = TempDir::new().expect("tempdir");
        let queue = queue_with(&dir, vod_entry("alice", "web:alice", RecordingTaskState::Running), Vec::new()).await;
        let app_config = app_config_with_quota(dir.path(), 1000);
        let gate = QuotaGate { queue: &queue, app_config: &app_config };

        let admitted = gate.admit_size("alice", None, 0).await.expect("admitted");
        assert_eq!(admitted.byte_cap, Some(1000));
    }

    #[tokio::test]
    async fn an_entry_waiting_to_attach_is_refused_for_its_own_quota() {
        // Bob queued behind Alice's transfer while its size was unknown.
        // Attaching later would charge him the whole file, so his quota has
        // to be asked now.
        let dir = TempDir::new().expect("tempdir");
        let queue = queue_with(
            &dir,
            vod_entry("alice", "web:alice", RecordingTaskState::Running),
            vec![vod_entry("bob", "web:bob", RecordingTaskState::Queued)],
        )
        .await;
        let app_config = app_config_with_quota(dir.path(), 4096);
        // Bob already holds something else that leaves him less than the file.
        crate::recording::recording_queue::mutate(&queue, |candidate| {
            let mut other = vod_entry("bob-other", "web:bob", RecordingTaskState::Completed);
            other.media_identity = "other".to_string();
            other.recording.measured_bytes = 1000;
            other.finished = true;
            candidate.finished.push(other);
            Ok(())
        })
        .await
        .expect("seed");
        let gate = QuotaGate { queue: &queue, app_config: &app_config };

        let admitted = gate.admit_size("alice", Some(4096), 0).await.expect("alice fits");
        assert!(admitted.siblings_refused);
        assert!(queue.queue.lock().await.is_empty(), "bob no longer waits");
        let bob = queue.finished.read().await.iter().find(|task| task.uuid == "bob").cloned().expect("bob is filed");
        assert_eq!(bob.state, RecordingTaskState::Failed);
        assert_eq!(bob.recording.reserved_bytes, 0);
    }

    #[test]
    fn a_slot_wait_maps_onto_the_acquisition_outcome() {
        // A wait that ends without an outcome ran into the live window's end;
        // being signalled means trying to acquire again.
        assert!(matches!(acquire_result_after_wait(None), Some(ProviderAcquireResult::WindowClosed)));
        assert!(acquire_result_after_wait(Some(RecordingWaitOutcome::Signalled)).is_none());
        assert!(matches!(
            acquire_result_after_wait(Some(RecordingWaitOutcome::Paused)),
            Some(ProviderAcquireResult::Paused)
        ));
        assert!(matches!(
            acquire_result_after_wait(Some(RecordingWaitOutcome::Cancelled)),
            Some(ProviderAcquireResult::Cancelled)
        ));
        assert!(matches!(
            acquire_result_after_wait(Some(RecordingWaitOutcome::Restarted)),
            Some(ProviderAcquireResult::Preempted)
        ));
        assert!(acquire_result_for_control(RecordingControl::None).is_none());
        assert!(matches!(acquire_result_for_control(RecordingControl::Cancel), Some(ProviderAcquireResult::Cancelled)));
        assert!(matches!(acquire_result_for_control(RecordingControl::Pause), Some(ProviderAcquireResult::Paused)));
        assert!(matches!(
            acquire_result_for_control(RecordingControl::Restart),
            Some(ProviderAcquireResult::Preempted)
        ));
    }

    #[test]
    fn a_live_window_is_measured_in_wall_clock_not_in_time_spent_recording() {
        // A capture that was preempted or waiting for capacity does not get
        // that time back: the broadcast ran regardless. The deadline is
        // anchored to the programme, so an interruption cannot push a capture
        // past the window and into the next programme.
        let now = chrono::Utc::now().timestamp();
        let started_ten_minutes_ago = scheduled_task(RecordingKind::Live, now - 600, 900);
        let remaining = recording_deadline_instant(&started_ten_minutes_ago)
            .expect("a scheduled live capture has a deadline")
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs();
        // 900s window, 600s already elapsed: about 300 left, never a fresh 900.
        assert!((295..=305).contains(&remaining), "expected roughly 300s left, got {remaining}");
    }

    #[test]
    fn a_live_window_that_has_already_closed_stops_immediately() {
        let now = chrono::Utc::now().timestamp();
        let long_over = scheduled_task(RecordingKind::Live, now - 7_200, 900);
        let deadline = recording_deadline_instant(&long_over).expect("still has a deadline");
        assert!(deadline <= tokio::time::Instant::now(), "a closed window must not keep recording");
    }

    #[test]
    fn only_live_captures_are_bounded_by_a_window() {
        // A film does not stop being downloadable because a clock ran out.
        let now = chrono::Utc::now().timestamp();
        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            assert!(recording_deadline_instant(&scheduled_task(kind, now, 900)).is_none());
        }
    }

    #[test]
    fn resumable_kinds_stage_through_a_partial_and_live_writes_in_place() {
        // The strategy split: an HTTP transfer can be interrupted and resumed,
        // so it stages. ffmpeg owns its output file for the whole capture and
        // has nothing to resume into.
        let now = chrono::Utc::now().timestamp();
        let live = scheduled_task(RecordingKind::Live, now, 900);
        assert_eq!(http_transfer_path(&live), live.file_path, "a live capture writes straight to its file");

        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            let task = scheduled_task(kind, now, 900);
            let staged = http_transfer_path(&task);
            assert_ne!(staged, task.file_path);
            assert!(
                staged.to_string_lossy().ends_with(".partial"),
                "a resumable transfer stages beside its final file, got {}",
                staged.display()
            );
        }
    }

    #[tokio::test]
    async fn stopping_live_capture_clears_cancel_before_starting_a_due_recording(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for terminal in [RecordingTaskState::Completed, RecordingTaskState::Failed] {
            for has_next in [false, true] {
                let dir = TempDir::new()?;
                let queue = RecordingQueue::new_persistent(dir.path(), dir.path())?;
                let now = chrono::Utc::now().timestamp();
                let mut active = scheduled_task(RecordingKind::Live, now - 60, 900);
                active.uuid = "immediate".to_string();
                let mut scheduled = scheduled_task(RecordingKind::Live, now, 900);
                scheduled.uuid = "scheduled".to_string();
                scheduled.state = RecordingTaskState::Scheduled;
                let active = RecordingQueue::to_persisted(&active);
                let scheduled = RecordingQueue::to_persisted(&scheduled);
                crate::recording::recording_queue::mutate(&queue, |candidate| {
                    candidate.active = vec![active];
                    if has_next {
                        candidate.scheduled.push(scheduled);
                    }
                    Ok(())
                })
                .await?;
                assert_eq!(queue.promote_due_scheduled(now).await, usize::from(has_next));
                assert_eq!(queue.cancel_requested("immediate").await?, Some(false));
                assert_eq!(*queue.worker("immediate").control_signal.read().await, RecordingControl::Cancel);

                let committed = finish_active_and_promote(&queue, "immediate", None, |task| {
                    task.state = terminal;
                    task.finished = true;
                    task.error =
                        (terminal == RecordingTaskState::Failed).then(|| "Stopped recording has no data".to_string());
                    task.recording.reserved_bytes = 0;
                    RecordingNotificationPlan::empty()
                })
                .await?;
                assert!(committed.is_some());
                assert_eq!(*queue.worker("immediate").control_signal.read().await, RecordingControl::None);
                assert_eq!(
                    queue.active.read().await.first().map(|task| task.uuid.as_str()),
                    has_next.then_some("scheduled")
                );
                let finished = queue.finished.read().await;
                assert_eq!(finished.len(), 1);
                assert_eq!(finished[0].state, terminal);
                assert!(finished[0].to_view(true).is_terminal());
                drop(finished);

                let restored = RecordingQueue::new_persistent(dir.path(), dir.path())?;
                restored.load_from_disk().await?;
                assert_eq!(restored.finished.read().await[0].state, terminal);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn finalizing_publishes_the_staged_bytes_and_clears_the_partial() {
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("film.mp4.partial");
        let final_path = dir.path().join("film.mp4");
        std::fs::write(&partial, b"complete recording").expect("write");

        finalize_http_transfer(&final_path, &partial).await.expect("finalize");

        assert!(final_path.exists());
        assert!(!partial.exists(), "the partial is cleared once published");
    }

    #[tokio::test]
    async fn finalizing_twice_succeeds_instead_of_failing_a_complete_recording() {
        // A crash between the link and the partial's removal leaves both files.
        // The resumed transfer sees a complete file, finalizes again, and used
        // to get AlreadyExists back -- reporting a recording whose bytes are
        // entirely on disk as failed, on every retry.
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("film.mp4.partial");
        let final_path = dir.path().join("film.mp4");
        std::fs::write(&partial, b"complete recording").expect("write");
        std::fs::hard_link(&partial, &final_path).expect("simulate the interrupted finalize");

        finalize_http_transfer(&final_path, &partial).await.expect("finalizing again must succeed");

        assert!(final_path.exists());
        assert!(!partial.exists(), "the second attempt finishes the job the first did not");
    }

    #[tokio::test]
    async fn a_different_file_at_the_final_path_is_not_published_over() {
        // Path reservation should make this unreachable. If it ever happens the
        // staged bytes were never verified against what is already there, so
        // the transfer must not claim success.
        let dir = TempDir::new().expect("tempdir");
        let partial = dir.path().join("film.mp4.partial");
        let final_path = dir.path().join("film.mp4");
        std::fs::write(&partial, b"the recording we just made").expect("write");
        std::fs::write(&final_path, b"something else entirely, of another size").expect("write");

        let error = finalize_http_transfer(&final_path, &partial).await.expect_err("must refuse");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(partial.exists(), "the staged bytes are kept for inspection");
    }

    #[tokio::test]
    async fn a_transfer_written_straight_to_its_final_path_needs_no_finalization() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("stream.ts");
        std::fs::write(&final_path, b"live capture").expect("write");

        finalize_http_transfer(&final_path, &final_path).await.expect("finalize");

        assert!(final_path.exists());
    }
    /// A queue with nothing in it; the wait queue is what these exercise.
    fn slot_queue(dir: &TempDir) -> RecordingQueue {
        RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository")
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_capture_stops_waiting_for_capacity_when_its_window_closes() {
        // Waiting past the padded end cannot produce the recording that was
        // asked for -- the programme has finished. Before this the wait had no
        // bound at all, so a live capture with no free slot sat in the queue
        // through its own broadcast and out the other side.
        let dir = TempDir::new().expect("tempdir");
        let queue = slot_queue(&dir);
        let control = RwLock::new(RecordingControl::None);
        let notify = Notify::new();
        let window_ends = tokio::time::Instant::now() + Duration::from_mins(30);

        let outcome =
            wait_for_provider_slot(&queue, &Arc::from("provider"), 0, &control, &notify, Some(window_ends)).await;

        assert!(outcome.is_none(), "the wait ends when the programme does");
        assert!(queue.slot_waiters.snapshots().is_empty(), "expired waits must deregister");
        assert!(tokio::time::Instant::now() >= window_ends, "and only then");
    }

    #[tokio::test(start_paused = true)]
    async fn a_transfer_waits_for_a_slot_however_long_it_takes() {
        // The counterpart, and the reason the bound is not applied to
        // everything: a file on a server is still there in an hour.
        let dir = TempDir::new().expect("tempdir");
        let queue = slot_queue(&dir);
        let control = RwLock::new(RecordingControl::None);
        let notify = Notify::new();

        let waiters = Arc::clone(&queue.slot_waiters);
        let freed = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_mins(60)).await;
            let queued = waiters.snapshots();
            let waiter = queued.first().expect("someone is waiting an hour later");
            waiters.signal_waiter(waiter.id)
        });

        let outcome = wait_for_provider_slot(&queue, &Arc::from("provider"), 0, &control, &notify, None).await;

        assert!(freed.await.expect("signaller"), "the waiter was still there to signal");
        assert!(matches!(outcome, Some(RecordingWaitOutcome::Signalled)), "it took the slot when one appeared");
    }

    #[tokio::test(start_paused = true)]
    async fn a_capacity_wait_still_answers_a_cancellation_before_its_deadline() {
        // The deadline must not swallow the control signals: a user cancelling
        // a live capture that is waiting for capacity should not have to wait
        // out the programme.
        let dir = TempDir::new().expect("tempdir");
        let queue = slot_queue(&dir);
        let control = RwLock::new(RecordingControl::None);
        let notify = Notify::new();
        let window_ends = tokio::time::Instant::now() + Duration::from_mins(30);

        let provider: Arc<str> = Arc::from("provider");
        let waiting = wait_for_provider_slot(&queue, &provider, 0, &control, &notify, Some(window_ends));
        let cancelling = async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            *control.write().await = RecordingControl::Cancel;
            notify.notify_waiters();
        };
        let (outcome, ()) = tokio::join!(waiting, cancelling);

        assert!(matches!(outcome, Some(RecordingWaitOutcome::Cancelled)), "the cancel is answered, not the deadline");
        assert!(tokio::time::Instant::now() < window_ends, "and long before the window closes");
    }
    fn bare_app_config() -> tuliprox_core::model::AppConfig {
        tuliprox_core::model::AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::Config::default())),
            sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
            hdhomerun: Arc::new(arc_swap::ArcSwapOption::default()),
            api_proxy: Arc::new(arc_swap::ArcSwapOption::default()),
            file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
            paths: Arc::new(arc_swap::ArcSwap::from_pointee(shared::model::ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(arc_swap::ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::new()),
        }
    }

    /// Configured local API listener with no public playback server.
    fn app_config_with_listener() -> tuliprox_core::model::AppConfig {
        let app_config = bare_app_config();
        let mut config = (*app_config.config.load_full()).clone();
        config.api.host = "127.0.0.1".to_string();
        config.api.port = 8901;
        app_config.config.store(Arc::new(config));
        app_config
    }

    /// An executable standing in for ffmpeg that appends a line to `log`
    /// every time it is run, then writes its output argument and exits
    /// cleanly. The log is what lets a test say "once" rather than "at
    /// least once".
    fn counting_ffmpeg(dir: &Path, log: &Path) -> PathBuf {
        let script_path = dir.join("fake-ffmpeg");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\necho run >> \"{}\"\nfor arg in \"$@\"; do output=\"$arg\"; done\nprintf 'recorded' > \"$output\"\nexit 0\n",
                log.display()
            ),
        )
        .expect("write fake ffmpeg");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).expect("chmod");
        }
        script_path
    }

    fn spawn_count(log: &Path) -> usize {
        std::fs::read_to_string(log).map_or(0, |text| text.lines().filter(|line| !line.is_empty()).count())
    }

    #[cfg(unix)]
    struct LimitedCapacity {
        limit: usize,
        in_use: AtomicUsize,
        peak: AtomicUsize,
        releases: AtomicUsize,
        notify: Arc<Notify>,
    }

    #[cfg(unix)]
    impl RecordingCapacityPort for LimitedCapacity {
        fn capacities_for_input<'a>(
            &'a self,
            input: &'a Arc<str>,
        ) -> futures::future::BoxFuture<'a, Vec<crate::recording::recording_capacity::ProviderCapacity>> {
            Box::pin(async move {
                let in_use = self.in_use.load(Ordering::SeqCst);
                tokio::task::yield_now().await;
                vec![(input.clone(), in_use, self.limit)]
            })
        }

        fn acquire<'a>(
            &'a self,
            _input: &'a Arc<str>,
            _priority: i8,
        ) -> futures::future::BoxFuture<'a, Option<tuliprox_core::model::ProviderHandle>> {
            Box::pin(async move {
                let previous = self
                    .in_use
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                        (self.limit == 0 || used < self.limit).then_some(used + 1)
                    })
                    .ok()?;
                self.peak.fetch_max(previous + 1, Ordering::SeqCst);
                Some(tuliprox_core::model::ProviderHandle::new(
                    std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                    1,
                    tuliprox_core::model::ProviderAllocation::Exhausted,
                    Some(tokio_util::sync::CancellationToken::new()),
                ))
            })
        }

        fn release(&self, handle: Option<tuliprox_core::model::ProviderHandle>) -> futures::future::BoxFuture<'_, ()> {
            Box::pin(async move {
                if handle.is_some() {
                    self.in_use.fetch_sub(1, Ordering::SeqCst);
                    self.releases.fetch_add(1, Ordering::SeqCst);
                    self.notify.notify_one();
                }
            })
        }

        fn capacity_changed(&self) -> Arc<Notify> { self.notify.clone() }
    }

    #[cfg(unix)]
    struct ConcurrentLiveFixture {
        dir: TempDir,
        queue: Arc<RecordingQueue>,
        script: PathBuf,
        capacity: Arc<LimitedCapacity>,
        app: Arc<tuliprox_core::model::AppConfig>,
        config: RecordingConfig,
        cancel: tokio_util::sync::CancellationToken,
    }

    #[cfg(unix)]
    impl ConcurrentLiveFixture {
        async fn new(count: u32, limit: usize) -> Result<Self, Box<dyn std::error::Error>> {
            Self::with_background_limit(count, limit, 0).await
        }

        async fn with_background_limit(
            count: u32,
            limit: usize,
            background_limit: u8,
        ) -> Result<Self, Box<dyn std::error::Error>> {
            let dir = TempDir::new()?;
            let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
            let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
            std::fs::write(&script, "#!/bin/sh\nfor arg in \"$@\"; do output=\"$arg\"; done\nprintf recorded > \"$output\"\ntouch \"$output.ready\"\nread -r control\nexit 0\n")?;
            let now = chrono::Utc::now().timestamp();
            let tasks = (0..count)
                .map(|index| {
                    let mut task = scheduled_task(RecordingKind::Live, now, 300);
                    task.uuid = format!("live-{index}");
                    task.state = RecordingTaskState::Queued;
                    task.priority = 5;
                    task.input_name = Some(Arc::from("provider"));
                    task.recording.source.virtual_id = (42 + index).to_string();
                    task.recording.source.input_name = "provider".to_string();
                    task.file_dir = dir.path().to_path_buf();
                    task.file_path = dir.path().join(format!("live-{index}.ts"));
                    RecordingQueue::to_persisted(&task)
                })
                .collect::<Vec<_>>();
            crate::recording::recording_queue::mutate(&queue, |candidate| {
                candidate.queue.extend(tasks);
                Ok(())
            })
            .await?;
            let fixture = Self {
                dir,
                queue,
                script,
                capacity: Arc::new(LimitedCapacity {
                    limit,
                    in_use: AtomicUsize::new(0),
                    peak: AtomicUsize::new(0),
                    releases: AtomicUsize::new(0),
                    notify: Arc::new(Notify::new()),
                }),
                app: Arc::new(app_config_with_listener()),
                config: RecordingConfig::from(&shared::model::RecordingConfigDto {
                    enabled: true,
                    max_background_per_provider: background_limit,
                    ..Default::default()
                }),
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            start_recording_scheduler(
                fixture.app.clone(),
                fixture.config.clone(),
                &fixture.queue,
                NoopSink,
                fixture.capacity.clone(),
                fixture.cancel.clone(),
                fixture.script.clone(),
            );
            Ok(fixture)
        }

        async fn start(&self) -> Result<(), String> {
            let capacity: Arc<dyn RecordingCapacityPort> = self.capacity.clone();
            ensure_recording_worker_running(&self.app, &self.config, &self.queue, &NoopSink, &capacity, &self.script)
                .await
        }

        async fn wait_for_running(&self, count: usize) -> Result<Vec<String>, tokio::time::error::Elapsed> {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let ready = self
                        .queue
                        .active
                        .read()
                        .await
                        .iter()
                        .filter(|task| {
                            task.state == RecordingTaskState::Running
                                && self.dir.path().join(format!("{}.ts.partial.ready", task.uuid)).exists()
                        })
                        .map(|task| task.uuid.clone())
                        .collect::<Vec<_>>();
                    if ready.len() == count {
                        break ready;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
        }

        async fn stop_all(&self, count: usize) -> Result<(), Box<dyn std::error::Error>> {
            let active = self.queue.active.read().await.iter().map(|task| task.uuid.clone()).collect::<Vec<_>>();
            for uuid in active {
                self.queue.cancel_requested(&uuid).await?;
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.queue.finished.read().await.len() != count || self.queue.workers_running().await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            assert_eq!(self.capacity.in_use.load(Ordering::SeqCst), 0);
            Ok(())
        }
    }

    #[cfg(unix)]
    impl Drop for ConcurrentLiveFixture {
        fn drop(&mut self) { self.cancel.cancel(); }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn independent_live_workers_fill_provider_capacity_and_stop_only_the_selected_recording(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for limit in [0, 4] {
            let fixture = ConcurrentLiveFixture::new(4, limit).await?;
            let (first, second) = tokio::join!(fixture.start(), fixture.start());
            first?;
            second?;
            fixture.wait_for_running(4).await?;
            assert_eq!(fixture.capacity.peak.load(Ordering::SeqCst), 4);
            fixture.queue.cancel_requested("live-2").await?;
            let remaining = fixture.wait_for_running(3).await?;
            assert!(!remaining.iter().any(|uuid| uuid == "live-2"));
            assert!(fixture
                .queue
                .active
                .read()
                .await
                .iter()
                .filter(|task| task.uuid != "live-2")
                .all(|task| task.state == RecordingTaskState::Running));
            fixture.stop_all(4).await?;
            assert_eq!(
                fixture.capacity.releases.load(Ordering::SeqCst),
                4,
                "one allocation per recording despite concurrent starts"
            );
            assert!(fixture.queue.finished.read().await.iter().all(|task| task.state == RecordingTaskState::Completed));
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_workers_respect_capacity_and_start_waiting_recordings_when_a_slot_is_released(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = ConcurrentLiveFixture::new(4, 2).await?;
        fixture.start().await?;
        let running = fixture.wait_for_running(2).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.queue.slot_waiters.snapshots().len() != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(fixture.capacity.in_use.load(Ordering::SeqCst), 2);
        fixture.queue.cancel_requested(&running[0]).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let now_running = fixture.wait_for_running(2).await?;
                if now_running.iter().any(|uuid| !running.contains(uuid)) {
                    break Ok::<_, tokio::time::error::Elapsed>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(fixture.capacity.peak.load(Ordering::SeqCst), 2);
        assert!(fixture
            .queue
            .active
            .read()
            .await
            .iter()
            .any(|task| task.uuid == running[1] && task.state == RecordingTaskState::Running));
        fixture.stop_all(4).await?;
        assert!(fixture.queue.finished.read().await.iter().all(|task| task.to_view(true).is_terminal()));
        assert!(fixture.queue.slot_waiters.snapshots().is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_starts_respect_the_configured_background_limit() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = ConcurrentLiveFixture::with_background_limit(3, 3, 1).await?;
        fixture.start().await?;
        let running = fixture.wait_for_running(1).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.queue.slot_waiters.snapshots().len() != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(fixture.capacity.peak.load(Ordering::SeqCst), 1);
        fixture.queue.cancel_requested(&running[0]).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let now_running = fixture.wait_for_running(1).await?;
                if now_running != running {
                    break Ok::<_, tokio::time::error::Elapsed>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(fixture.capacity.peak.load(Ordering::SeqCst), 1);
        fixture.stop_all(3).await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scheduler_starts_a_due_live_capture_while_another_capture_is_running(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for has_data in [false, true] {
            let dir = TempDir::new()?;
            let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
            let now = chrono::Utc::now().timestamp();
            let mut immediate = scheduled_task(RecordingKind::Live, now - 60, 900);
            immediate.uuid = "immediate".to_string();
            immediate.state = RecordingTaskState::Queued;
            immediate.input_name = Some(Arc::from("provider"));
            immediate.recording.source.virtual_id = "42".to_string();
            immediate.file_dir = dir.path().to_path_buf();
            immediate.file_path = dir.path().join("immediate.ts");
            let mut scheduled = immediate.clone();
            scheduled.uuid = "scheduled".to_string();
            scheduled.state = RecordingTaskState::Scheduled;
            scheduled.file_path = dir.path().join("scheduled.ts");
            scheduled.recording.program_start = Some(now);
            scheduled.recording.scheduled_start = Some(now);
            let immediate = RecordingQueue::to_persisted(&immediate);
            let scheduled = RecordingQueue::to_persisted(&scheduled);
            crate::recording::recording_queue::mutate(&queue, |candidate| {
                candidate.queue.push(immediate);
                candidate.scheduled.push(scheduled);
                Ok(())
            })
            .await?;
            let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
            let write_initial = if has_data { "printf recorded > \"$output\"" } else { ":" };
            std::fs::write(&script, format!(
                "#!/bin/sh\nfor arg in \"$@\"; do output=\"$arg\"; done\ncase \"$output\" in\n*immediate.ts.partial) {write_initial}; touch \"$output.ready\"; read -r control ;;\n*) printf recorded > \"$output\" ;;\nesac\nexit 0\n"
            ))?;
            let stub = StubCapacity::with_room();
            let capacity: Arc<dyn RecordingCapacityPort> = stub.clone();
            let config =
                RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() });
            ensure_recording_worker_running(
                &app_config_with_listener(),
                &config,
                &queue,
                &NoopSink,
                &capacity,
                &script,
            )
            .await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                while !dir.path().join("immediate.ts.partial.ready").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            let cancel = tokio_util::sync::CancellationToken::new();
            start_recording_scheduler(
                Arc::new(app_config_with_listener()),
                config.clone(),
                &queue,
                NoopSink,
                capacity.clone(),
                cancel.clone(),
                script.clone(),
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while !queue.finished.read().await.iter().any(|task| task.uuid == "scheduled") {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            assert!(queue
                .active
                .read()
                .await
                .iter()
                .any(|task| task.uuid == "immediate" && task.state == RecordingTaskState::Running));
            assert!(queue.finished.read().await.iter().all(|task| task.uuid != "immediate"));
            queue.cancel_requested("immediate").await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                while queue.finished.read().await.len() != 2 || queue.workers_running().await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            cancel.cancel();
            assert_eq!(stub.acquire_count(), 2);
            let finished = queue.finished.read().await;
            let stopped = finished.iter().find(|task| task.uuid == "immediate").ok_or("missing stopped task")?;
            let next = finished.iter().find(|task| task.uuid == "scheduled").ok_or("missing scheduled task")?;
            assert_eq!(
                stopped.state,
                if has_data { RecordingTaskState::Completed } else { RecordingTaskState::Failed }
            );
            assert_eq!(next.state, RecordingTaskState::Completed, "{:?}", next.error);
            assert_eq!(tokio::fs::read(&next.file_path).await?, b"recorded");
            assert!(stopped.to_view(true).is_terminal());
            assert!(next.to_view(true).is_terminal());
            assert!(queue.active.read().await.is_empty());
            assert_eq!(*queue.worker("immediate").control_signal.read().await, RecordingControl::None);
            assert_eq!(stub.release_count(), 2);
        }
        Ok(())
    }

    #[tokio::test]
    async fn live_timeout_keeps_the_transfer_cause_without_claiming_the_window_expired(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path())?);
        let mut capture = scheduled_task(RecordingKind::Live, chrono::Utc::now().timestamp(), 300);
        capture.recording.source.virtual_id = "42".to_string();
        capture.file_dir = dir.path().to_path_buf();
        capture.file_path = dir.path().join("capture.ts");
        capture.state = RecordingTaskState::Queued;
        capture.input_name = Some(Arc::from("provider"));
        let persisted = RecordingQueue::to_persisted(&capture);
        crate::recording::recording_queue::mutate(&queue, move |candidate| {
            candidate.queue.push(persisted.clone());
            Ok(())
        })
        .await?;
        let script = counting_ffmpeg(dir.path(), &dir.path().join("spawns.log"));
        std::fs::write(&script, "#!/bin/sh\necho 'Error opening input files: Operation timed out' >&2\nexit 1\n")?;
        let stub = StubCapacity::with_room();
        let capacity: Arc<dyn RecordingCapacityPort> = Arc::clone(&stub) as Arc<dyn RecordingCapacityPort>;
        ensure_recording_worker_running(
            &app_config_with_listener(),
            &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
            &queue,
            &NoopSink,
            &capacity,
            &script,
        )
        .await?;
        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(task) = queue.finished.read().await.first().cloned() {
                    break task;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(settled.state, RecordingTaskState::Failed);
        let error = settled.error.as_deref().ok_or("missing transfer cause")?;
        assert!(error.contains("Error opening input files: Operation timed out"), "{error}");
        assert!(!error.contains("window has moved on"), "{error}");
        assert_eq!(settled.retry_attempts, 1);
        assert_eq!(settled.recording.reserved_bytes, 0);
        assert_eq!(stub.acquire_count(), 1);
        assert_eq!(stub.release_count(), 1);
        assert!(queue.active.read().await.is_empty());
        assert_eq!(queue.finished.read().await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn media_retry_waiting_and_limit_keep_the_transfer_cause() -> Result<(), Box<dyn std::error::Error>> {
        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            let queue = RecordingQueue::new();
            let task = scheduled_task(kind, chrono::Utc::now().timestamp(), 300);
            let persisted = RecordingQueue::to_persisted(&task);
            crate::recording::recording_queue::mutate(&queue, move |candidate| {
                candidate.active = vec![persisted.clone()];
                Ok(())
            })
            .await?;
            let config = RecordingConfig::from(&shared::model::RecordingConfigDto {
                retry_max_attempts: 1,
                retry_backoff_initial_secs: 1,
                retry_backoff_max_secs: 1,
                retry_backoff_jitter_percent: 0,
                ..Default::default()
            });
            let cause = "Error while opening stream: (http://user:password@upstream.example/live/1?token=secret) Operation timed out";
            let public_cause = crate::recording::recording_worker::redact_url_tokens(cause);
            assert_eq!(public_cause, "Error while opening stream: [stream URL] Operation timed out");
            let retry = super::prepare_active_retry(&queue, "task", &config, &public_cause).await?;
            assert!(matches!(retry, Some(super::RetryCommit::Waiting { delay_secs: 1, attempts: 1 })));
            {
                let active = queue.active.read().await;
                let active = active.first().ok_or("active transfer missing")?;
                assert_eq!(active.state, RecordingTaskState::RetryWaiting);
                assert!(active.error.as_deref().is_some_and(|error| error.contains(&public_cause)));
                assert!(active.next_retry_at.is_some());
            }
            let retry = super::prepare_active_retry(&queue, "task", &config, &public_cause).await?;
            assert!(matches!(retry, Some(super::RetryCommit::Failed(_))));
            let finished = queue.finished.read().await;
            let failed = finished.first().ok_or("terminal transfer missing")?;
            assert_eq!(failed.state, RecordingTaskState::Failed);
            assert_eq!(
                failed.error.as_deref(),
                Some(format!("Retry limit reached after 1 attempts: {public_cause}").as_str())
            );
            assert_eq!(failed.recording.reserved_bytes, 0);
            assert!(failed.next_retry_at.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    async fn scheduler_starts_a_queued_transfer_after_worker_exit() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let mut transfer = scheduled_task(RecordingKind::Vod, chrono::Utc::now().timestamp(), 300);
        transfer.state = RecordingTaskState::Queued;
        transfer.input_name = Some(Arc::from("provider"));
        let persisted = RecordingQueue::to_persisted(&transfer);
        crate::recording::recording_queue::mutate(&queue, move |candidate| {
            candidate.queue.push(persisted.clone());
            Ok(())
        })
        .await
        .expect("seed queue");

        let capacity: Arc<dyn RecordingCapacityPort> = StubCapacity::full();
        let cancel = tokio_util::sync::CancellationToken::new();
        start_recording_scheduler(
            Arc::new(bare_app_config()),
            RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
            &queue,
            NoopSink,
            capacity,
            cancel.clone(),
            Path::new(crate::recording::recording_worker::FFMPEG_BINARY).to_path_buf(),
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if queue
                    .active
                    .read()
                    .await
                    .first()
                    .is_some_and(|task| task.state == RecordingTaskState::WaitingForCapacity)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scheduler did not restart the stranded transfer");
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_capture_that_never_gets_capacity_fails_at_its_window_without_recording() {
        // The whole path, driven against a provider that is permanently full:
        // the worker asks for a slot, is refused, waits, and gives up when the
        // programme it was going to record has finished. Until the capacity
        // port existed this could only be produced by a real provider actually
        // being busy for the length of a broadcast.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let now = chrono::Utc::now().timestamp();
        let mut capture = scheduled_task(RecordingKind::Live, now, 1_800);
        capture.uuid = "live".to_string();
        capture.state = RecordingTaskState::Queued;
        capture.input_name = Some(Arc::from("provider"));
        let capture = RecordingQueue::to_persisted(&capture);
        crate::recording::recording_queue::mutate(&queue, move |candidate| {
            candidate.queue.push(capture.clone());
            Ok(())
        })
        .await
        .expect("seed");

        let stub = StubCapacity::full();
        let capacity: Arc<dyn RecordingCapacityPort> = Arc::clone(&stub) as Arc<dyn RecordingCapacityPort>;
        let app_config = bare_app_config();
        ensure_recording_worker_running(
            &app_config,
            &RecordingConfig::from(&shared::model::RecordingConfigDto {
                enabled: true,
                ..shared::model::RecordingConfigDto::default()
            }),
            &queue,
            &NoopSink,
            &capacity,
            Path::new(crate::recording::recording_worker::FFMPEG_BINARY),
        )
        .await
        .expect("worker started");

        // Sleeping rather than spinning: with a paused clock the runtime only
        // advances time when nothing is runnable, so a busy wait would stop the
        // window from ever closing.
        tokio::time::sleep(Duration::from_mins(31)).await;

        let finished = queue.finished.read().await;
        let settled = finished.first().expect("the capture reached a terminal state");
        assert_eq!(settled.state, RecordingTaskState::Failed, "no capacity ever came");
        assert_eq!(settled.error.as_deref(), Some(LIVE_CAPACITY_WINDOW_CLOSED), "and it says why");
        assert_eq!(settled.size, 0, "nothing was ever recorded");
        assert_eq!(settled.recording.reserved_bytes, 0, "and it is not still holding disk");
        assert_eq!(
            stub.acquire_count(),
            1,
            "it asked for a slot once and then waited, rather than spinning on the provider"
        );
        assert_eq!(stub.release_count(), 0, "and it never held one to give back");
    }

    #[tokio::test]
    async fn a_live_capture_asks_for_capacity_once_runs_the_encoder_once_and_completes_once() {
        // The other half of the window contract. A real clock here on
        // purpose: the worker spawns an actual child process, and pausing
        // time would let unrelated timers fire between the spawn and the
        // exit while the process itself still ran in wall-clock.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let recordings_dir = dir.path().join("recordings");
        std::fs::create_dir_all(&recordings_dir).expect("create recording dir");
        let log = dir.path().join("spawns.log");
        let script = counting_ffmpeg(dir.path(), &log);

        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let now = chrono::Utc::now().timestamp();
        let mut capture = scheduled_task(RecordingKind::Live, now, 1_800);
        capture.uuid = "live".to_string();
        capture.state = RecordingTaskState::Queued;
        capture.input_name = Some(Arc::from("provider"));
        // The execution URL is derived from the source, so the virtual id has
        // to be a real one rather than the placeholder the other fixtures use.
        capture.recording.source.virtual_id = "42".to_string();
        capture.file_dir.clone_from(&recordings_dir);
        capture.file_path = recordings_dir.join("capture.ts");
        let persisted = RecordingQueue::to_persisted(&capture);
        crate::recording::recording_queue::mutate(&queue, move |candidate| {
            candidate.queue.push(persisted.clone());
            Ok(())
        })
        .await
        .expect("seed");

        let stub = StubCapacity::with_room();
        let capacity: Arc<dyn RecordingCapacityPort> = Arc::clone(&stub) as Arc<dyn RecordingCapacityPort>;
        ensure_recording_worker_running(
            &app_config_with_listener(),
            &RecordingConfig::from(&shared::model::RecordingConfigDto {
                enabled: true,
                ..shared::model::RecordingConfigDto::default()
            }),
            &queue,
            &NoopSink,
            &capacity,
            &script,
        )
        .await
        .expect("worker started");

        let settled = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(done) = queue.finished.read().await.first().cloned() {
                    break done;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        // A bare timeout here says nothing about why. Where the capture got
        // stuck is the whole diagnosis, so report it.
        let Ok(settled) = settled else {
            panic!(
                "never settled: active={:?} queued={} spawns={} acquires={}",
                queue.active.read().await.first().map(|active| (
                    active.uuid.clone(),
                    active.state,
                    active.error.clone()
                )),
                queue.queue.lock().await.len(),
                spawn_count(&log),
                stub.acquire_count(),
            );
        };

        assert_eq!(settled.state, RecordingTaskState::Completed, "{:?}", settled.error);
        assert_eq!(spawn_count(&log), 1, "the encoder ran once, not once per retry or per promotion pass");
        assert_eq!(stub.acquire_count(), 1, "and one slot was asked for");
        assert_eq!(stub.release_count(), 1, "and given back exactly once");
        assert_eq!(queue.finished.read().await.len(), 1, "the recording was committed once");
        assert_eq!(
            tokio::fs::read(&capture.file_path).await.expect("the recording is where playback will look"),
            b"recorded"
        );
        assert!(!crate::recording_partial_path(&capture.file_path).exists(), "and nothing was left staged");
        assert_eq!(settled.recording.reserved_bytes, 0, "and it is not still holding disk");
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> std::io::Result<String> {
        use tokio::io::AsyncReadExt;
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte).await? == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "incomplete request headers"));
            }
            request.push(byte[0]);
        }
        Ok(String::from_utf8_lossy(&request).to_ascii_lowercase())
    }

    #[tokio::test]
    async fn recording_redirects_keep_the_proxy_and_resume_headers() -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_url = reqwest::Url::parse(&format!("http://{}/capture", listener.local_addr()?))?;
        let local_task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let request = read_request(&mut socket).await?;
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://recording-origin.invalid/start\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            Ok::<_, std::io::Error>(request)
        });
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let proxy_url = format!("http://{}", proxy.local_addr()?);
        let proxy_task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in [
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: /finish\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 302 Found\r\nLocation: http://recording-cdn.invalid/end\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-9/10\r\nContent-Length: 6\r\nConnection: close\r\n\r\n456789",
            ] {
                let (mut socket, _) = proxy.accept().await?;
                let request = read_request(&mut socket).await?;
                socket.write_all(response.as_bytes()).await?;
                requests.push(request);
            }
            Ok::<_, std::io::Error>(requests)
        });
        let config = app_config_with_listener();
        let mut updated = (*config.config.load_full()).clone();
        updated.proxy = Some(tuliprox_core::model::ProxyConfig { url: proxy_url, username: None, password: None });
        config.config.store(Arc::new(updated));
        let mut sensitive = reqwest::header::HeaderMap::new();
        sensitive.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer listener-secret"),
        );
        sensitive.insert(reqwest::header::COOKIE, reqwest::header::HeaderValue::from_static("session=listener-secret"));
        let local = tuliprox_core::utils::request::create_client(&config)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(sensitive)
            .build()?;
        let recording_headers = std::collections::HashMap::from([
            ("Authorization".to_string(), "Bearer recording-secret".to_string()),
            ("Cookie".to_string(), "session=recording-secret".to_string()),
            ("X-Recording-Test".to_string(), "custom".to_string()),
        ]);
        let upstream = tuliprox_core::utils::request::create_client(&config)
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(tuliprox_core::utils::request::get_request_headers(
                Some(&recording_headers),
                None,
                None,
                None,
            ))
            .build()?;
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            super::send_download_request(
                &local,
                Some(&upstream),
                &local_url,
                4,
                &RwLock::new(RecordingControl::None),
                &Notify::new(),
            ),
        )
        .await?
        .map_err(|result| format!("request failed: {result:?}"))?;
        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.bytes().await?.as_ref(), b"456789");
        let local_request = local_task.await??;
        let upstream_requests = proxy_task.await??;
        assert!(local_request.starts_with("get /capture "));
        assert!(upstream_requests[0].starts_with("get http://recording-origin.invalid/start "));
        assert!(upstream_requests[1].starts_with("get http://recording-origin.invalid/finish "));
        assert!(local_request.contains("authorization: bearer listener-secret"), "{local_request}");
        assert!(local_request.contains("cookie: session=listener-secret"), "{local_request}");
        assert!(!local_request.contains("recording-secret"), "{local_request}");
        assert!(upstream_requests[2].starts_with("get http://recording-cdn.invalid/end "));
        for (index, request) in upstream_requests.iter().enumerate() {
            assert!(!request.contains("listener-secret"), "{request}");
            if index < 2 {
                assert!(request.contains("authorization: bearer recording-secret"), "{request}");
                assert!(request.contains("cookie: session=recording-secret"), "{request}");
            } else {
                assert!(!request.contains("recording-secret"), "{request}");
            }
            assert!(request.contains("x-recording-test: custom"), "{request}");
        }
        for request in std::iter::once(&local_request).chain(&upstream_requests) {
            assert!(request.contains("range: bytes=4-"), "{request}");
            assert!(request.contains("accept-encoding: identity"), "{request}");
            assert!(request.contains(&shared::model::RECORDING_STREAM_USER_AGENT.to_ascii_lowercase()), "{request}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn recording_redirects_without_upstream_client_strip_listener_credentials(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_url = reqwest::Url::parse(&format!("http://{}/capture", listener.local_addr()?))?;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            upstream.local_addr()?
        );
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (listener, response) in [
                (listener, redirect),
                (upstream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()),
            ] {
                let (mut socket, _) = listener.accept().await?;
                let request = read_request(&mut socket).await?;
                socket.write_all(response.as_bytes()).await?;
                requests.push(request);
            }
            Ok::<_, std::io::Error>(requests)
        });
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer listener-secret"),
        );
        headers.insert(reqwest::header::COOKIE, reqwest::header::HeaderValue::from_static("session=listener-secret"));
        headers.insert("x-recording-test", reqwest::header::HeaderValue::from_static("custom"));
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()?;
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            super::send_download_request(
                &client,
                None,
                &local_url,
                0,
                &RwLock::new(RecordingControl::None),
                &Notify::new(),
            ),
        )
        .await?
        .map_err(|result| format!("request failed: {result:?}"))?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let requests = server.await??;
        assert!(requests[0].contains("authorization: bearer listener-secret"), "{}", requests[0]);
        assert!(requests[0].contains("cookie: session=listener-secret"), "{}", requests[0]);
        assert!(!requests[1].contains("listener-secret"), "{}", requests[1]);
        assert!(requests[1].contains("x-recording-test: custom"), "{}", requests[1]);
        Ok(())
    }

    #[tokio::test]
    async fn serial_transfers_start_the_next_worker_without_waiting_for_a_tick(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::AsyncWriteExt;
        let dir = TempDir::new()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let app = Arc::new(app_config_with_listener());
        let mut config = (*app.config.load_full()).clone();
        config.api.port = listener.local_addr()?.port();
        app.config.store(Arc::new(config));
        let queue = Arc::new(RecordingQueue::new());
        for index in 0..2 {
            let mut task = scheduled_task(RecordingKind::Vod, chrono::Utc::now().timestamp(), 300);
            task.uuid = format!("transfer-{index}");
            task.state = RecordingTaskState::Queued;
            task.recording.source.virtual_id = (42 + index).to_string();
            task.file_dir = dir.path().to_path_buf();
            task.file_path = dir.path().join(format!("transfer-{index}.mp4"));
            queue.queue.lock().await.push_back(task);
        }
        let (first_seen, first_received) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let (second_seen, second_received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for (seen, gate) in [(first_seen, Some(released)), (second_seen, None)] {
                let (mut socket, _) = listener.accept().await?;
                read_request(&mut socket).await?;
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n").await?;
                let _ = seen.send(());
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
                socket.write_all(b"0123456789").await?;
            }
            Ok::<_, std::io::Error>(())
        });
        let cancellation = tokio_util::sync::CancellationToken::new();
        let capacity: Arc<dyn RecordingCapacityPort> = StubCapacity::with_room();
        start_recording_scheduler(
            app,
            RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
            &queue,
            NoopSink,
            capacity,
            cancellation.clone(),
            PathBuf::from("unused"),
        );
        let result = async {
            tokio::time::timeout(Duration::from_secs(5), first_received).await??;
            assert_eq!(queue.active.read().await.len(), 1);
            assert_eq!(queue.queue.lock().await.len(), 1);
            tokio::time::sleep(Duration::from_millis(50)).await;
            release.send(()).map_err(|()| "fixture stopped")?;
            tokio::time::timeout(Duration::from_millis(500), second_received).await??;
            tokio::time::timeout(Duration::from_secs(5), async {
                while queue.finished.read().await.len() != 2 || queue.workers_running().await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            assert!(queue.finished.read().await.iter().all(|task| task.state == RecordingTaskState::Completed));
            server.await??;
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        cancellation.cancel();
        result
    }

    #[tokio::test]
    async fn blocked_serial_queue_does_not_clone_partitions_on_each_commit() -> Result<(), Box<dyn std::error::Error>> {
        let queue = RecordingQueue::new();
        queue.active.write().await.push(scheduled_task(RecordingKind::Vod, 0, 300));
        for index in 0..49 {
            let mut task = scheduled_task(RecordingKind::Vod, 0, 300);
            task.uuid = format!("waiting-{index}");
            task.recording.source.virtual_id = index.to_string();
            task.state = RecordingTaskState::Queued;
            queue.queue.lock().await.push_back(task);
        }
        for index in 0..100 {
            let mut done = scheduled_task(RecordingKind::Vod, 0, 300);
            done.uuid = format!("completed-{index}");
            done.recording.source.virtual_id = (100 + index).to_string();
            done.state = RecordingTaskState::Completed;
            queue.finished.write().await.push(done);
        }
        for size in 0..3 {
            crate::recording::recording_queue::mutate(&queue, |candidate| {
                candidate.active[0].size = size;
                Ok(())
            })
            .await?;
            // Promotion has no reason to read this partition; a full snapshot does.
            let scheduled = queue.scheduled.write().await;
            assert!(!tokio::time::timeout(Duration::from_millis(200), super::promote_ready_downloads(&queue)).await??);
            drop(scheduled);
        }
        assert_eq!(queue.queue.lock().await.len(), 49);
        Ok(())
    }

    #[tokio::test]
    async fn idle_scheduler_does_not_snapshot_recording_history() -> Result<(), Box<dyn std::error::Error>> {
        let queue = Arc::new(RecordingQueue::new());
        let history = queue.finished.write().await;
        let capacity: Arc<dyn RecordingCapacityPort> = StubCapacity::with_room();
        let config = RecordingConfig::from(&shared::model::RecordingConfigDto::default());
        tokio::time::timeout(Duration::from_millis(200), async {
            queue.promote_due_scheduled_now().await;
            ensure_recording_worker_running(
                &app_config_with_listener(),
                &config,
                &queue,
                &NoopSink,
                &capacity,
                Path::new("unused"),
            )
            .await
        })
        .await??;
        drop(history);
        Ok(())
    }

    #[tokio::test]
    async fn vod_and_series_transfers_reach_the_local_listener_past_a_configured_proxy() {
        for kind in [RecordingKind::Vod, RecordingKind::Series] {
            let dir = tempfile::TempDir::new().expect("tempdir");
            let (fixture_url, server) = serve_range_fixture(true);
            let app_config = app_config_with_listener();
            let mut config = (*app_config.config.load_full()).clone();
            config.api.port = fixture_url.port().expect("fixture port");
            // Nothing listens here: a transfer routed through the proxy fails.
            config.proxy = Some(tuliprox_core::model::ProxyConfig {
                url: "http://127.0.0.1:9".to_string(),
                username: None,
                password: None,
            });
            app_config.config.store(Arc::new(config));

            let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
            let mut transfer = scheduled_task(kind, chrono::Utc::now().timestamp(), 300);
            transfer.state = RecordingTaskState::Queued;
            transfer.input_name = Some(Arc::from("provider"));
            transfer.recording.source.virtual_id = "42".to_string();
            transfer.file_dir = dir.path().to_path_buf();
            transfer.file_path = dir.path().join("transfer.mp4");
            let persisted = RecordingQueue::to_persisted(&transfer);
            crate::recording::recording_queue::mutate(&queue, move |candidate| {
                candidate.queue.push(persisted.clone());
                Ok(())
            })
            .await
            .expect("seed");

            let capacity: Arc<dyn RecordingCapacityPort> = StubCapacity::with_room() as Arc<dyn RecordingCapacityPort>;
            ensure_recording_worker_running(
                &app_config,
                &RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() }),
                &queue,
                &NoopSink,
                &capacity,
                Path::new(crate::recording::recording_worker::FFMPEG_BINARY),
            )
            .await
            .expect("worker started");

            let settled = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(done) = queue.finished.read().await.first().cloned() {
                        break done;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("transfer settled");
            assert_eq!(settled.state, RecordingTaskState::Completed, "{kind:?}: {:?}", settled.error);
            let request = server.join().expect("fixture server");
            assert!(request.starts_with("GET /api/v1/playlist/recording/"), "{request}");
        }
    }

    #[tokio::test]
    async fn a_recording_whose_disk_filled_while_it_waited_never_opens_a_destination() {
        // Admission happened when the request was accepted; this recording
        // then waited. By the time it reaches the front of the queue the
        // space it was admitted against is gone. Without the recheck the
        // first sign of that is a write failure part-way through.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let recordings_dir = dir.path().join("recordings");
        std::fs::create_dir_all(&recordings_dir).expect("create recording dir");
        let log = dir.path().join("spawns.log");
        let script = counting_ffmpeg(dir.path(), &log);

        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open repository"));
        let now = chrono::Utc::now().timestamp();
        let mut capture = scheduled_task(RecordingKind::Live, now, 1_800);
        capture.uuid = "live".to_string();
        capture.state = RecordingTaskState::Queued;
        capture.input_name = Some(Arc::from("provider"));
        capture.recording.source.virtual_id = "42".to_string();
        capture.recording.reserved_bytes = 1_024;
        capture.file_dir.clone_from(&recordings_dir);
        capture.file_path = recordings_dir.join("capture.ts");
        let persisted = RecordingQueue::to_persisted(&capture);
        crate::recording::recording_queue::mutate(&queue, move |candidate| {
            candidate.queue.push(persisted.clone());
            Ok(())
        })
        .await
        .expect("seed");

        // The safety margin stands in for a disk that filled up: it drives
        // headroom to zero without needing a real full filesystem.
        let app_config = app_config_with_listener();
        let mut rec_cfg =
            RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() });
        rec_cfg.directory = recordings_dir.to_string_lossy().into_owned();
        rec_cfg.disk = Some(tuliprox_core::model::RecordingDiskConfig {
            high_water_percent: None,
            low_water_percent: None,
            cleanup_interval_secs: None,
            safety_bytes: Some(u64::MAX),
        });
        let mut config = tuliprox_core::model::Config::clone(&app_config.config.load());
        config.video = Some(tuliprox_core::model::VideoConfig {
            extensions: Vec::new(),
            web_search: None,
            recording: Some(rec_cfg.clone()),
        });
        app_config.config.store(Arc::new(config));

        let stub = StubCapacity::with_room();
        let capacity: Arc<dyn RecordingCapacityPort> = Arc::clone(&stub) as Arc<dyn RecordingCapacityPort>;
        ensure_recording_worker_running(&app_config, &rec_cfg, &queue, &NoopSink, &capacity, &script)
            .await
            .expect("worker started");

        let settled = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(done) = queue.finished.read().await.first().cloned() {
                    break done;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let Ok(settled) = settled else {
            panic!(
                "never settled: active={:?} spawns={}",
                queue.active.read().await.first().map(|active| (active.state, active.error.clone())),
                spawn_count(&log),
            );
        };

        assert_eq!(settled.state, RecordingTaskState::Failed, "{:?}", settled.error);
        assert_eq!(settled.error.as_deref(), Some(DISK_GONE_BEFORE_START), "and it says which resource ran out");
        assert_eq!(spawn_count(&log), 0, "the encoder was never started");
        assert!(!capture.file_path.exists(), "and no destination was opened");
        assert_eq!(stub.release_count(), 1, "the provider slot it had was given back");
        assert_eq!(settled.recording.reserved_bytes, 0, "and it is not still holding disk");
    }
}
