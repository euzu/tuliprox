use crate::{
    api::{
        library_scan::{spawn_library_scan, LibraryScanTaskOptions},
        model::AppState,
    },
    model::{AppConfig, ProcessTargets, ScheduleConfig},
    processing::geoip::{update_geoip_db, GeoIpUpdateError},
    processing::processor::exec_processing,
    utils::exit,
};
use chrono::{DateTime, FixedOffset, Local};
use cron::Schedule;
use shared::{model::ScheduleTaskType, utils::{interner_gc, interner_len}};
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

pub fn exec_scheduler(
    client: &reqwest::Client,
    app_state: &Arc<AppState>,
    targets: &Arc<ProcessTargets>,
    cancel: &CancellationToken,
) {
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
            log::info!(
                "Skipping disabled scheduled task {:?} ({})",
                schedule.task_type,
                schedule.schedule
            );
            continue;
        }

        let expression = schedule.schedule.clone();
        let task_type = schedule.task_type;
        let exec_targets = get_process_targets(cfg, targets, schedule.targets.as_ref());
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
                let worker_targets = exec_targets;
                tokio::spawn(async move {
                    run_playlist_update_worker(worker_client, worker_state, worker_targets, rx, cancel_token).await;
                });
            }
            ScheduleTaskType::LibraryScan | ScheduleTaskType::GeoIpUpdate => {
                tokio::spawn(async move {
                    start_scheduler(
                        http_client,
                        expression.as_str(),
                        task_type,
                        app_state_clone,
                        exec_targets,
                        cancel_token,
                    )
                    .await;
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
    targets: Arc<ProcessTargets>,
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
                run_playlist_update_inner(&client, &app_state, &targets).await;
            }
        }
    }
}

async fn run_playlist_update_inner(client: &reqwest::Client, app_state: &Arc<AppState>, targets: &Arc<ProcessTargets>) {
    exec_processing(
        client,
        Arc::clone(&app_state.app_config),
        Arc::clone(targets),
        Some(Arc::clone(&app_state.event_manager)),
        Some(Arc::clone(app_state)),
        Some(app_state.playlists.clone()),
        Some(app_state.update_guard.clone()),
        app_state.get_disabled_headers(),
        Some(Arc::clone(&app_state.active_provider)),
        Some(Arc::clone(&app_state.metadata_manager)),
        None,
        None,
    )
    .await;
}

async fn start_scheduler(
    client: reqwest::Client,
    expression: &str,
    task_type: ScheduleTaskType,
    app_state: Arc<AppState>,
    _targets: Arc<ProcessTargets>,
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
                                    run_geoip_update(&app_state);
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

fn run_geoip_update(app_state: &Arc<AppState>) {
    let app_state = Arc::clone(app_state);
    tokio::spawn(async move {
        if let Err(err) = update_geoip_db(&app_state).await {
            if !matches!(err, GeoIpUpdateError::Disabled) {
                log::error!("Scheduled GeoIp update failed: {err}");
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
                return Arc::new(user_targets);
            }

            let inputs: Vec<u16> =
                user_targets.inputs.iter().filter(|&id| process_targets.inputs.contains(id)).copied().collect();
            let targets: Vec<u16> =
                user_targets.targets.iter().filter(|&id| process_targets.targets.contains(id)).copied().collect();
            let target_names: Vec<String> = user_targets
                .target_names
                .iter()
                .filter(|&name| process_targets.target_names.contains(name))
                .cloned()
                .collect();
            return Arc::new(ProcessTargets { enabled: user_targets.enabled, inputs, targets, target_names });
        }
    }
    Arc::clone(process_targets)
}

// TODO Consider making the GC interval configurable.
// The 180-second interval is hardcoded. For deployments with different memory/performance characteristics, a configurable interval might be useful.

/// Minimum number of interned strings required before GC runs.
/// Below this threshold the write-lock overhead outweighs the benefit.
const INTERNER_GC_MIN_POOL_SIZE: usize = 100;

pub fn exec_interner_prune(app_state: &Arc<AppState>) {
    let app_state = Arc::clone(app_state);
    tokio::spawn({
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(180)).await;
                // Skip GC entirely when the pool is too small — acquiring a write
                // lock on the global interner briefly blocks all concurrent interns,
                // so the cost only pays off when there are enough strings to clean.
                if interner_len() < INTERNER_GC_MIN_POOL_SIZE {
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
    use crate::api::scheduler::datetime_to_instant;
    use chrono::Local;
    use cron::Schedule;
    use std::{
        str::FromStr,
        sync::atomic::{AtomicU8, Ordering},
    };

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
}
