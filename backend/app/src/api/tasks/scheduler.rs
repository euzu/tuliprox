use crate::{
    api::{
        model::AppState,
        tasks::{spawn_library_scan, LibraryScanTaskOptions},
    },
    model::{AppConfig, ProcessTargets, ScheduleConfig},
    processing::{
        geoip::{update_geoip_db, GeoIpUpdateError},
        processor::{exec_processing, ProcessingRun},
    },
    utils::exit,
};
use chrono::{DateTime, FixedOffset, Local};
use cron::Schedule;
use shared::{
    model::{EventMessage, ScheduleTaskType, ScheduledTaskFailure},
    utils::{interner_gc, interner_len},
};
use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub fn datetime_to_instant(datetime: DateTime<FixedOffset>) -> Instant {
    // Convert DateTime<FixedOffset> to SystemTime
    let target_system_time: SystemTime = datetime.into();

    // Get the current SystemTime
    let now_system_time = SystemTime::now();

    // Calculate the duration between now and the target time
    let duration_until = target_system_time.duration_since(now_system_time).unwrap_or_else(|_| Duration::from_secs(0));

    // Get the current Instant and add the duration to calculate the target Instant
    Instant::now() + duration_until
}

pub fn exec_scheduler(client: &reqwest::Client, app_state: &Arc<AppState>, cancel: &CancellationToken) {
    let cfg = &app_state.app_config;
    let config = cfg.config.load();
    let schedules: Vec<ScheduleConfig> =
        if let Some(schedules) = &config.schedules { schedules.clone() } else { vec![] };
    for schedule in schedules {
        let task_enabled = match schedule.task_type {
            ScheduleTaskType::PlaylistUpdate => true,
            ScheduleTaskType::LibraryScan => config.library.as_ref().is_some_and(|library| library.enabled),
            ScheduleTaskType::GeoIpUpdate => config
                .reverse_proxy
                .as_ref()
                .and_then(|reverse_proxy| reverse_proxy.geoip.as_ref())
                .is_some_and(|geoip| geoip.enabled),
        };
        if !task_enabled {
            log::info!("Skipping disabled scheduled task {:?} ({})", schedule.task_type, schedule.schedule);
            continue;
        }

        let expression = schedule.schedule.clone();
        let task_type = schedule.task_type;
        // Store the schedule's target names (not resolved IDs) so we can
        // re-resolve against current sources at each execution.
        let schedule_target_names: Option<Vec<String>> = schedule.targets.clone();
        let app_state_clone = Arc::clone(app_state);
        let http_client = client.clone();
        let cancel_token = cancel.clone();

        match task_type {
            ScheduleTaskType::PlaylistUpdate => {
                // Bounded channel with capacity 1: if an update is already pending or
                // running and the scheduler fires again, the extra signal is dropped
                // (deduplicated).  This prevents redundant runs from piling up when
                // updates are slow or blocked waiting for the playlist lock.
                let (tx, rx) = mpsc::channel::<()>(1);

                // Cron trigger: fires at scheduled times and notifies the worker.
                let trigger_cancel = cancel_token.clone();
                tokio::spawn(async move {
                    start_playlist_trigger(expression.as_str(), trigger_cancel, tx).await;
                });

                // Worker: processes triggers one at a time, blocking on the playlist
                // lock when another update is active.  Cancels cleanly on shutdown.
                let worker_client = http_client;
                let worker_state = app_state_clone;
                tokio::spawn(async move {
                    Box::pin(run_playlist_update_worker(
                        worker_client,
                        worker_state,
                        schedule_target_names,
                        rx,
                        cancel_token,
                    ))
                    .await;
                });
            }
            ScheduleTaskType::LibraryScan | ScheduleTaskType::GeoIpUpdate => {
                tokio::spawn(async move {
                    start_scheduler(http_client, expression.as_str(), task_type, app_state_clone, cancel_token).await;
                });
            }
        }
    }
}

/// Cron trigger for playlist updates.  Fires at each scheduled time and sends a
/// unit signal to the worker via a bounded channel.  If the channel is already
/// full (one update is pending), `try_send` silently drops the signal —
/// deduplication at zero cost.
async fn start_playlist_trigger(expression: &str, cancel: CancellationToken, tx: mpsc::Sender<()>) {
    match Schedule::from_str(expression) {
        Ok(schedule) => {
            let offset = *Local::now().offset();
            loop {
                let mut upcoming = schedule.upcoming(offset).take(1);
                if let Some(datetime) = upcoming.next() {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        () = tokio::time::sleep_until(tokio::time::Instant::from(datetime_to_instant(datetime))) => {
                            // If the channel is full, there is already one pending run queued.
                            // This fire is covered by that pending run; drop it.
                            let _ = tx.try_send(());
                        }
                    }
                }
            }
        }
        Err(err) => exit!("Failed to start scheduler: {err}"),
    }
}

/// Worker for playlist updates.  Waits for trigger signals from the cron trigger
/// and runs updates one at a time.  Blocks on `acquire_playlist_lock` while
/// another update source (manual trigger, metadata update, another schedule) holds
/// the lock — the worker resumes automatically once the lock is released.
async fn run_playlist_update_worker(
    client: reqwest::Client,
    app_state: Arc<AppState>,
    schedule_target_names: Option<Vec<String>>,
    mut rx: mpsc::Receiver<()>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            msg = rx.recv() => {
                if msg.is_none() {
                    break; // Sender dropped (scheduler task exited)
                }
                run_playlist_update_inner(&client, &app_state, schedule_target_names.as_ref()).await;
                if !drain_pending_playlist_triggers(&mut rx) {
                    break;
                }
            }
        }
    }
}

pub fn drain_pending_playlist_triggers(rx: &mut mpsc::Receiver<()>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(()) => {
                log::debug!("Scheduled playlist update coalesced with an already-completed update");
            }
            Err(mpsc::error::TryRecvError::Empty) => return true,
            Err(mpsc::error::TryRecvError::Disconnected) => return false,
        }
    }
}

async fn run_playlist_update_inner(
    client: &reqwest::Client,
    app_state: &Arc<AppState>,
    schedule_target_names: Option<&Vec<String>>,
) {
    let Some(permit) = app_state.update_guard.acquire_playlist_lock().await else {
        return;
    };
    // Re-resolve targets from the CURRENT sources and forced_targets each time,
    // so that input/target ID changes from hot-reloads are picked up.
    let targets = get_process_targets(&app_state.app_config, &app_state.forced_targets.load(), schedule_target_names);
    exec_processing(
        ProcessingRun::new(
            client.clone(),
            Arc::clone(&app_state.app_config),
            targets,
            Arc::clone(&app_state.event_manager),
        )
        .with_bootstrap({
            let state = Arc::clone(app_state);
            move || {
                let state = Arc::clone(&state);
                async move { crate::api::sync_panel_api_exp_dates(&state).await }
            }
        })
        .with_playlist_state(app_state.playlists.clone())
        .with_update_guard(app_state.update_guard.clone())
        .with_disabled_headers(app_state.get_disabled_headers())
        .with_provider_manager(Arc::clone(&app_state.active_provider))
        .with_metadata_manager(Arc::clone(&app_state.metadata_manager))
        .with_acquired_permit(permit),
    )
    .await;
}

async fn start_scheduler(
    client: reqwest::Client,
    expression: &str,
    task_type: ScheduleTaskType,
    app_state: Arc<AppState>,
    cancel: CancellationToken,
) {
    match Schedule::from_str(expression) {
        Ok(schedule) => {
            let offset = *Local::now().offset();
            loop {
                let mut upcoming = schedule.upcoming(offset).take(1);
                if let Some(datetime) = upcoming.next() {
                    tokio::select! {
                        () = tokio::time::sleep_until(tokio::time::Instant::from(datetime_to_instant(datetime))) => {
                            match task_type {
                                ScheduleTaskType::PlaylistUpdate => unreachable!("handled by channel-based path"),
                                ScheduleTaskType::LibraryScan => {
                                    run_library_scan(&client, &app_state);
                                }
                                ScheduleTaskType::GeoIpUpdate => {
                                    run_geoip_update(&app_state, expression);
                                }
                            }
                        }
                        () = cancel.cancelled() => {
                            break;
                        }
                    }
                }
            }
        }
        Err(err) => exit!("Failed to start scheduler: {err}"),
    }
}

fn run_library_scan(client: &reqwest::Client, app_state: &Arc<AppState>) {
    let config = app_state.app_config.config.load();
    if let Some(lib_config) = config.library.as_ref() {
        if lib_config.enabled {
            if let Some(permit) = app_state.update_guard.try_library() {
                let event_manager = Arc::clone(&app_state.event_manager);
                spawn_library_scan(
                    event_manager,
                    lib_config.clone(),
                    config.metadata_update.clone(),
                    client.clone(),
                    LibraryScanTaskOptions {
                        force_rescan: false,
                        message_prefix: "Scheduled ",
                        storage_dir: config.storage_dir.clone(),
                    },
                    permit,
                );
            }
        }
    }
}

fn run_geoip_update(app_state: &Arc<AppState>, schedule: &str) {
    let app_state = Arc::clone(app_state);
    let schedule = schedule.to_string();
    tokio::spawn(async move {
        if let Err(err) = update_geoip_db(&app_state.app_config, &app_state.http_client.load(), &app_state.geoip).await
        {
            // `Disabled` is not a failure - the task ran and found nothing to
            // do, which is what the config asked for.
            if !matches!(err, GeoIpUpdateError::Disabled) {
                log::error!("Scheduled GeoIp update failed: {err}");
                // The playlist update and the library scan both report their
                // own outcomes. This one had no terminal event of its own, so
                // an operator running on a stale database never found out.
                let _ = app_state.event_manager.send_event(EventMessage::ScheduledTaskFailed(
                    ScheduledTaskFailure::new(ScheduleTaskType::GeoIpUpdate, err.to_string()).with_schedule(schedule),
                ));
            }
        }
    });
}

pub fn get_process_targets(
    cfg: &Arc<AppConfig>,
    process_targets: &Arc<ProcessTargets>,
    exec_targets: Option<&Vec<String>>,
) -> Arc<ProcessTargets> {
    let sources = cfg.sources.load();
    if let Ok(user_targets) = sources.validate_targets(exec_targets) {
        if user_targets.enabled {
            if !process_targets.enabled {
                // No CLI filters in place, use schedule's targets directly
                return Arc::new(user_targets);
            }

            // CLI filters (-t flag) are in place, filter targets only
            // Do NOT filter inputs, they come from the schedule's validate_targets result
            let targets: Vec<u16> =
                user_targets.targets.iter().filter(|&id| process_targets.targets.contains(id)).copied().collect();
            let target_names: Vec<String> = user_targets
                .target_names
                .iter()
                .filter(|&name| process_targets.target_names.contains(name))
                .cloned()
                .collect();

            return Arc::new(ProcessTargets {
                enabled: user_targets.enabled,
                inputs: user_targets.inputs, // preserve from validate_targets, no filtering
                targets,
                target_names,
            });
        }
    }
    Arc::clone(process_targets)
}

pub fn exec_interner_prune(app_state: &Arc<AppState>) {
    let app_state = Arc::clone(app_state);
    tokio::spawn({
        async move {
            loop {
                let (interval_secs, min_pool_size) = {
                    let config = app_state.app_config.config.load();
                    (
                        u64::from(config.interner_gc_interval_secs),
                        usize::try_from(config.interner_gc_min_pool_size).unwrap_or(usize::MAX),
                    )
                };
                tokio::time::sleep(Duration::from_secs(interval_secs)).await;
                // Skip GC entirely when the pool is too small — acquiring a write
                // lock on the global interner briefly blocks all concurrent interns,
                // so the cost only pays off when there are enough strings to clean.
                if interner_len() < min_pool_size {
                    continue;
                }
                if let Some(permit) = app_state.update_guard.try_playlist() {
                    // Gate check: ensure updates aren't in progress; permit dropped to allow concurrent updates during GC
                    drop(permit);
                    interner_gc();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use crate::api::tasks::{datetime_to_instant, drain_pending_playlist_triggers};
    use chrono::Local;
    use cron::Schedule;
    use std::{
        str::FromStr,
        sync::atomic::{AtomicU8, Ordering},
    };
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn drops_triggers_accumulated_during_update() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(()).await.expect("initial trigger");
        assert!(rx.recv().await.is_some());
        assert!(tx.try_send(()).is_ok());

        assert!(drain_pending_playlist_triggers(&mut rx));
        assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        assert!(tx.try_send(()).is_ok());
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn test_run_scheduler() {
        // Define a cron expression that runs every second
        let expression = "0/1 * * * * * *"; // every second

        let runs = AtomicU8::new(0);
        let run_me = || runs.fetch_add(1, Ordering::AcqRel);

        let start = std::time::Instant::now();
        if let Ok(schedule) = Schedule::from_str(expression) {
            let offset = *Local::now().offset();
            loop {
                let mut upcoming = schedule.upcoming(offset).take(1);
                if let Some(datetime) = upcoming.next() {
                    tokio::time::sleep_until(tokio::time::Instant::from(datetime_to_instant(datetime))).await;
                    run_me();
                }
                if runs.load(Ordering::Acquire) == 6 {
                    break;
                }
            }
        }
        let duration = start.elapsed();

        assert!(runs.load(Ordering::Acquire) == 6, "Failed to run");
        assert!(duration.as_secs() > 4, "Failed time");
    }

    #[test]
    fn test_get_process_targets_preserves_inputs_from_schedule() {
        // Documents the expected behavior for the hot-reload silent failure scenario:
        //
        // 1. Server starts without `-t` CLI flag
        //    -> forced_targets = ProcessTargets { enabled: false, inputs: [], targets: [] }
        //
        // 2. Schedule has targets: ["my-target"]
        //    -> validate_targets returns: ProcessTargets { enabled: true, inputs: [1,2,3], targets: [100] }
        //
        // 3. get_process_targets(cfg, &forced_targets, Some(&["my-target"]))
        //    should return: ProcessTargets { enabled: true, inputs: [1,2,3], targets: [100] }
        //    NOT: ProcessTargets { enabled: true, inputs: [], targets: [100] }
        //         (which would have been produced by the old code filtering inputs against forced_targets.inputs)
        //
        // A full integration test requires an AppConfig populated with sources and targets.
    }
}
