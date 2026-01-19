use crate::api::model::AppState;
use crate::api::panel_api::sync_panel_api_exp_dates_on_boot;
use crate::model::{AppConfig, ProcessTargets, ScheduleConfig};
use crate::processing::processor::playlist::exec_processing;
use crate::utils::exit;
use chrono::{DateTime, FixedOffset, Local};
use cron::Schedule;
use log::{error};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio_util::sync::CancellationToken;
use shared::utils::interner_gc;

pub fn datetime_to_instant(datetime: DateTime<FixedOffset>) -> Instant {
    // Convert DateTime<FixedOffset> to SystemTime
    let target_system_time: SystemTime = datetime.into();

    // Get the current SystemTime
    let now_system_time = SystemTime::now();

    // Calculate the duration between now and the target time
    let duration_until = target_system_time
        .duration_since(now_system_time)
        .unwrap_or_else(|_| Duration::from_secs(0));

    // Get the current Instant and add the duration to calculate the target Instant
    Instant::now() + duration_until
}

pub fn exec_scheduler(client: &reqwest::Client, app_state: &Arc<AppState>, targets: &Arc<ProcessTargets>,
                      cancel: &CancellationToken) {
    let cfg = &app_state.app_config;
    let config = cfg.config.load();
    let schedules: Vec<ScheduleConfig> = if let Some(schedules) = &config.schedules {
        schedules.clone()
    } else {
        vec![]
    };
    for schedule in schedules {
        let expression = schedule.schedule.clone();
        let exec_targets = get_process_targets(cfg, targets, schedule.targets.as_ref());
        let app_state_clone = Arc::clone(app_state);
        let http_client = client.clone();
        let cancel_token = cancel.clone();
        tokio::spawn(async move {
            start_scheduler(http_client, expression.as_str(), app_state_clone, exec_targets, cancel_token).await;
        });
    }
}

async fn start_scheduler(client: reqwest::Client, expression: &str, app_state: Arc<AppState>,
                         targets: Arc<ProcessTargets>, cancel: CancellationToken) {
    match Schedule::from_str(expression) {
        Ok(schedule) => {
            let offset = *Local::now().offset();
            loop {
                let mut upcoming = schedule.upcoming(offset).take(1);
                if let Some(datetime) = upcoming.next() {
                    tokio::select! {
                        () = tokio::time::sleep_until(tokio::time::Instant::from(datetime_to_instant(datetime))) => {
                       let app_config = Arc::clone(&app_state.app_config);
                       let event_manager = Arc::clone(&app_state.event_manager);
                       let playlist_state = app_state.playlists.clone();
                       sync_panel_api_exp_dates_on_boot(&app_state).await;
                       exec_processing(&client, app_config, Arc::clone(&targets), Some(event_manager),
                            Some(playlist_state), Some(app_state.update_guard.clone())).await;
                        }
                        () = cancel.cancelled() => {
                            break;
                        }
                    }
                }
            }
        }
        Err(err) => exit!("Failed to start scheduler: {}", err)
    }
}

fn get_process_targets(cfg: &Arc<AppConfig>, process_targets: &Arc<ProcessTargets>, exec_targets: Option<&Vec<String>>) -> Arc<ProcessTargets> {
    let sources = cfg.sources.load();
    if let Ok(user_targets) = sources.validate_targets(exec_targets) {
        if user_targets.enabled {
            if !process_targets.enabled {
                return Arc::new(user_targets);
            }

            let inputs: Vec<u16> = user_targets.inputs.iter()
                .filter(|&id| process_targets.inputs.contains(id))
                .copied()
                .collect();
            let targets: Vec<u16> = user_targets.targets.iter()
                .filter(|&id| process_targets.inputs.contains(id))
                .copied()
                .collect();
            let target_names: Vec<String> = user_targets.target_names.iter()
                .filter(|&name| process_targets.target_names.contains(name))
                .cloned()
                .collect();
            return Arc::new(ProcessTargets {
                enabled: user_targets.enabled,
                inputs,
                targets,
                target_names,
            });
        }
    }
    Arc::clone(process_targets)
}

// TODO Consider making the GC interval configurable.
// The 180-second interval is hardcoded. For deployments with different memory/performance characteristics, a configurable interval might be useful.
pub fn exec_interner_prune(app_state: &Arc<AppState>) {
    let app_state = Arc::clone(app_state);
    tokio::spawn({
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(180)).await;
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
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU8, Ordering};

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
