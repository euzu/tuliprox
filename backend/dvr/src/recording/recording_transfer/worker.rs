use super::{
    acquire_result_after_wait, acquire_result_for_control, active_download_snapshot_for_worker,
    background_download_should_wait, broadcast_required_worker_mutation, broadcast_worker_mutation,
    cancel_active_and_promote, capacities_have_free_slot, commit_acquired_download, commit_failed_active_download,
    continue_after_pause, download_file, finish_active_and_promote, mark_recording_metadata_notification,
    preemption_reason_for, prepare_active_retry, promote_ready_downloads, publish_recording_change,
    recording_deadline_instant, recording_execution_download, refresh_recording_progress, refused_before_start,
    requeue_active_download_for_retry, set_active_download_state, should_exit_worker_after_preempt,
    spawn_recording_notification_after_persist, take_active, wait_for_provider_slot, write_completion_sidecar,
    ProviderAcquireResult, ProviderCapacities, QuotaGate, RecordingNotificationPlan, RetryCommit,
    DOWNLOAD_PREEMPTED_REASON, LIVE_CAPACITY_WINDOW_CLOSED, MAINTENANCE_REASON, RECORDING_PROGRESS_UPDATE_INTERVAL,
};
use crate::recording::{
    recording_capacity::RecordingCapacityPort,
    recording_ctx::RecordingCtx,
    recording_notification::LifecycleEvent,
    recording_queue::{
        mutate_optional, PersistedRecordingQueue, QueueMutationError, RecordingControl, RecordingQueue,
        RecordingTaskState,
    },
    recording_worker::{
        recording_partial_path, redact_url_tokens, run_recording_with_binary, RecordingExecutionResult,
    },
};
use log::{debug, error, info, warn};
use shared::model::{EventMessage, EventSink, RecordingFinishedState, RecordingKind};
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::{time, time::Duration};
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{AppConfig, RecordingConfig},
    utils::{request, request::create_client},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DownloadExecutionResult {
    Completed,
    Paused,
    Cancelled,
    Preempted,
    SharedJoinUnavailable { message: String, retryable: bool },
    Retryable(String),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadRetryDecision<'a> {
    Retry(&'a str),
    Failed(&'a str),
}

pub(super) fn download_retry_decision(result: &DownloadExecutionResult) -> Option<DownloadRetryDecision<'_>> {
    match result {
        DownloadExecutionResult::Retryable(err)
        | DownloadExecutionResult::SharedJoinUnavailable { message: err, retryable: true } => {
            Some(DownloadRetryDecision::Retry(err.as_str()))
        }
        DownloadExecutionResult::Failed(err)
        | DownloadExecutionResult::SharedJoinUnavailable { message: err, retryable: false } => {
            Some(DownloadRetryDecision::Failed(err.as_str()))
        }
        DownloadExecutionResult::Completed
        | DownloadExecutionResult::Paused
        | DownloadExecutionResult::Cancelled
        | DownloadExecutionResult::Preempted => None,
    }
}

pub(super) async fn requeue_active_download_for_capacity_wait(
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

async fn fail_active_download_worker<E: EventSink + Clone>(
    dq: &RecordingQueue,
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    event_manager: &E,
    uuid: &str,
    reason: &str,
) -> Result<bool, QueueMutationError> {
    let clone_metadata = event_manager.has_event_receivers();
    let committed = commit_failed_active_download(dq, uuid, reason, clone_metadata).await?;
    if let Some(commit) = committed {
        spawn_recording_notification_after_persist(app_config, client, commit.notification, true);
        publish_recording_change(event_manager);
        if let Some(metadata) = commit.metadata {
            event_manager.emit(EventMessage::RecordingFinished {
                id: uuid.to_string(),
                state: RecordingFinishedState::Failed,
                metadata,
            });
        }
        Ok(true)
    } else {
        Ok(false)
    }
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
                let mut attempt_shared_join = true;
                'worker: loop {
                    // One read: the uuid, pause flag and live window must describe
                    // the same task.
                    let active_head =
                        dq.active.read().await.iter().find(|task| task.uuid == worker_uuid).map(|download| {
                            // A retry or a requeue of a recording that already ran is not a new
                            // start; maintenance only refuses new ones.
                            let first_start = !download.recording.provider_started;
                            (
                                download.paused,
                                recording_deadline_instant(download),
                                first_start,
                                download.kind,
                                download.recording.source.clone(),
                                download.file_path.clone(),
                            )
                        });
                    if let Some((paused, window_deadline, first_start, download_kind, source, file_path)) = active_head
                    {
                        if paused {
                            break;
                        }

                        let (input_name, priority) =
                            dq.active_scheduling_priority(&worker_uuid).await.unwrap_or((None, 0i8));

                        let should_try_shared_join =
                            attempt_shared_join && first_start && download_kind == RecordingKind::Live && {
                                let sources = app_config.sources.load();
                                sources
                                    .get_target_by_name(&source.target_id)
                                    .and_then(|t| t.options.as_ref())
                                    .is_some_and(shared::model::ConfigTargetOptions::share_live_mpeg_ts_enabled)
                            };

                        let can_join_shared = if should_try_shared_join {
                            let virtual_id = source.virtual_id.parse::<u32>().unwrap_or(0);
                            capacity
                                .shared_stream_available(
                                    &source.target_id,
                                    &source.input_name,
                                    virtual_id,
                                    source.cluster,
                                )
                                .await
                        } else {
                            false
                        };

                        // Acquire a provider connection slot for this download.
                        // If the provider is at capacity, wait in the priority queue until signalled.
                        // Never proceeds without a slot when input_name is set — account bans otherwise.
                        let (provider_acquire_result, joined_shared) = if can_join_shared {
                            (ProviderAcquireResult::Acquired(None), true)
                        } else {
                            let acquire_res = if let Some(input_name) = input_name {
                                loop {
                                    if let Some(stopped) = acquire_result_for_control(*control_signal.read().await) {
                                        break stopped;
                                    }
                                    let handle = {
                                        // Keep the policy check and allocation together so
                                        // concurrent recording starts respect background limits.
                                        let _capacity = dq.capacity_guard.lock().await;
                                        let capacities = capacity.capacities_for_input(&input_name).await;
                                        // Checked right before every slot request, after every await
                                        // of this attempt, so nothing has to be released.
                                        if first_start && capacity.is_input_in_maintenance(&input_name) {
                                            break ProviderAcquireResult::Maintenance;
                                        }
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
                            };
                            (acquire_res, false)
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
                                if let Err(err) = fail_active_download_worker(
                                    &dq,
                                    &app_config,
                                    &client,
                                    &event_manager,
                                    &worker_uuid,
                                    LIVE_CAPACITY_WINDOW_CLOSED,
                                )
                                .await
                                {
                                    error!("Download worker commit failed: {err}");
                                    break 'worker;
                                }
                                continue;
                            }
                            ProviderAcquireResult::Maintenance => {
                                if let Err(err) = fail_active_download_worker(
                                    &dq,
                                    &app_config,
                                    &client,
                                    &event_manager,
                                    &worker_uuid,
                                    MAINTENANCE_REASON,
                                )
                                .await
                                {
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
                                if let Err(err) = fail_active_download_worker(
                                    &dq,
                                    &app_config,
                                    &client,
                                    &event_manager,
                                    &worker_uuid,
                                    reason,
                                )
                                .await
                                {
                                    error!("Download worker commit failed: {err}");
                                    break 'worker;
                                }
                                continue;
                            }
                            match download.kind {
                                RecordingKind::Vod | RecordingKind::Series => 'http_execution: {
                                    let execution_download = match recording_execution_download(
                                        &app_config,
                                        &download,
                                        provider_handle.as_ref(),
                                        false,
                                        None,
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
                                        joined_shared,
                                        Some(priority),
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
                                        RecordingExecutionResult::SharedJoinUnavailable { message, retryable } => {
                                            DownloadExecutionResult::SharedJoinUnavailable { message, retryable }
                                        }
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

                        if joined_shared {
                            let failed_with_503 =
                                matches!(&execution_result, DownloadExecutionResult::SharedJoinUnavailable { .. });
                            // Later attempts never try the shared join again.
                            attempt_shared_join = false;
                            let progress_path = recording_partial_path(&file_path);
                            // Restart on an own slot only when the join captured nothing: a
                            // shared stream that ended mid-recording keeps its partial file
                            // and goes through the normal retry handling below.
                            let captured_nothing =
                                tokio::fs::metadata(&progress_path).await.map_or(true, |meta| meta.len() == 0);
                            if failed_with_503 && captured_nothing {
                                let _ = tokio::fs::remove_file(&progress_path).await;
                                let _ = mutate_optional(&dq, |candidate| {
                                    if let Some(active) =
                                        candidate.active.iter_mut().find(|task| task.uuid == worker_uuid)
                                    {
                                        active.recording.provider_started = false;
                                        Ok(Some(true))
                                    } else {
                                        Ok(None)
                                    }
                                })
                                .await;
                                continue 'worker;
                            }
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
                                        app_config.live_api_user_recording_quotas(),
                                    )
                                });
                                let mut completed_meta = None;
                                let committed =
                                    finish_active_and_promote(&dq, &worker_uuid, completion_limits.as_ref(), |fd| {
                                        fd.finished = true;
                                        fd.paused = false;
                                        fd.size = measured_bytes;
                                        fd.next_retry_at = None;
                                        let meta = &mut fd.recording;
                                        meta.measured_bytes = measured_bytes;
                                        meta.reserved_bytes = 0;
                                        meta.completed_at = Some(chrono::Utc::now().timestamp());
                                        meta.partial_relative_path = None;
                                        if fd.cancelled_by_owner {
                                            fd.state = RecordingTaskState::Cancelled;
                                            fd.error = Some("Cancelled by user".to_string());
                                            RecordingNotificationPlan::empty()
                                        } else {
                                            fd.state = RecordingTaskState::Completed;
                                            fd.error = None;
                                            completed_meta = if event_manager.has_event_receivers() {
                                                Some(Arc::new(fd.recording.clone()))
                                            } else {
                                                None
                                            };
                                            mark_recording_metadata_notification(
                                                &mut fd.recording,
                                                LifecycleEvent::Completed,
                                                None,
                                            )
                                        }
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
                                        if let Some(meta) = completed_meta {
                                            event_manager.emit(EventMessage::RecordingFinished {
                                                id: worker_uuid.clone(),
                                                state: RecordingFinishedState::Completed,
                                                metadata: meta,
                                            });
                                        }
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
                            other => match download_retry_decision(&other) {
                                Some(DownloadRetryDecision::Retry(err)) => {
                                    capacity.release(provider_handle).await;
                                    let err = redact_url_tokens(err);
                                    warn!("Recording transfer encountered a transient failure: {err}");
                                    let retry_commit = prepare_active_retry(
                                        &dq,
                                        &worker_uuid,
                                        &download_cfg,
                                        &err,
                                        event_manager.has_event_receivers(),
                                    )
                                    .await;
                                    let retry_delay_secs = match retry_commit {
                                        Ok(Some(RetryCommit::Waiting { delay_secs, attempts })) => {
                                            debug!("Download retry attempt {attempts} scheduled in {delay_secs}s");
                                            publish_recording_change(&event_manager);
                                            delay_secs
                                        }
                                        Ok(Some(RetryCommit::Failed { notification, metadata })) => {
                                            spawn_recording_notification_after_persist(
                                                &app_config,
                                                &client,
                                                notification,
                                                true,
                                            );
                                            publish_recording_change(&event_manager);
                                            if let Some(meta) = metadata {
                                                event_manager.emit(EventMessage::RecordingFinished {
                                                    id: worker_uuid.clone(),
                                                    state: RecordingFinishedState::Failed,
                                                    metadata: meta,
                                                });
                                            }
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
                                        DownloadExecutionResult::Retryable(_)
                                        | DownloadExecutionResult::SharedJoinUnavailable { retryable: true, .. } => {
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
                                        DownloadExecutionResult::Completed
                                        | DownloadExecutionResult::Failed(_)
                                        | DownloadExecutionResult::SharedJoinUnavailable { retryable: false, .. } => {}
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
                                Some(DownloadRetryDecision::Failed(err)) => {
                                    capacity.release(provider_handle).await;
                                    let err = redact_url_tokens(err);
                                    warn!("Download failed permanently: {err}");
                                    if let Err(commit_err) = fail_active_download_worker(
                                        &dq,
                                        &app_config,
                                        &client,
                                        &event_manager,
                                        &worker_uuid,
                                        &err,
                                    )
                                    .await
                                    {
                                        error!("Failed to persist failed download state: {commit_err}");
                                        break 'worker;
                                    }
                                }
                                None => {}
                            },
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

pub(super) fn start_recording_scheduler<E: EventSink + Clone + 'static>(
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
    use super::*;

    #[test]
    fn download_retry_decision_maps_execution_result_to_retry_or_failed() {
        let unavail_failed = DownloadExecutionResult::SharedJoinUnavailable {
            message: "HTTP 404 Not Found channel_unavailable".to_string(),
            retryable: false,
        };
        let unavail_retry = DownloadExecutionResult::SharedJoinUnavailable {
            message: "HTTP 503 Service Unavailable".to_string(),
            retryable: true,
        };
        let regular_retry = DownloadExecutionResult::Retryable("Connection timed out".to_string());
        let regular_failed = DownloadExecutionResult::Failed("HTTP 404 Not Found".to_string());

        assert_eq!(
            download_retry_decision(&unavail_failed),
            Some(DownloadRetryDecision::Failed("HTTP 404 Not Found channel_unavailable"))
        );
        assert_eq!(
            download_retry_decision(&unavail_retry),
            Some(DownloadRetryDecision::Retry("HTTP 503 Service Unavailable"))
        );
        assert_eq!(download_retry_decision(&regular_retry), Some(DownloadRetryDecision::Retry("Connection timed out")));
        assert_eq!(download_retry_decision(&regular_failed), Some(DownloadRetryDecision::Failed("HTTP 404 Not Found")));
        assert_eq!(download_retry_decision(&DownloadExecutionResult::Completed), None);
        assert_eq!(download_retry_decision(&DownloadExecutionResult::Paused), None);
        assert_eq!(download_retry_decision(&DownloadExecutionResult::Cancelled), None);
        assert_eq!(download_retry_decision(&DownloadExecutionResult::Preempted), None);
    }
}
