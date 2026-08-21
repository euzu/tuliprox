//! Retention supervisor.
//!
//! One task drives both sweeps so they can never delete concurrently:
//!
//! - the **policy sweep** (`keep_last_per_channel` / `delete_after_days`)
//!   runs every `retention.sweep_interval_secs`;
//! - the **disk-pressure sweep** runs on the shorter
//!   `disk.cleanup_interval_secs` cadence, and only actually measures the
//!   filesystem when at least [`MIN_DISK_PRESSURE_INTERVAL_SECS`] have
//!   passed since the last measurement.
//!
//! Both sweeps delete through
//! [`RecordingService::system_retention_delete`](crate::api::model::recording_service::RecordingService::system_retention_delete),
//! so there is exactly one deletion path in the system.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info};
use shared::model::Claims;
use tokio_util::sync::CancellationToken;

use crate::api::model::recording_service::{RecordingService, ServiceError};
use crate::api::model::recording_worker_runner::{DeleteOutcome, DiskConfig};
use crate::api::model::{AppState, EventMessage};

use super::health::{supervisor_health, SupervisorHealth};
use super::{
    now_ts, recording_config, recording_enabled, system_claims, PassGuard,
};

/// Floor on how often the recording root is measured. `statvfs` is cheap
/// but not free, and it would be wasteful to re-measure on a tick that
/// fires seconds after the last one (which a small
/// `disk.cleanup_interval_secs` would do).
const MIN_DISK_PRESSURE_INTERVAL_SECS: u64 = 30;

/// Fallback watermark-check cadence when `disk.cleanup_interval_secs` is
/// unset.
const DEFAULT_WATERMARK_CHECK_INTERVAL_SECS: u64 = 60;

/// Start the retention supervisor.
pub fn spawn_retention_supervisor(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    let app_state = Arc::clone(app_state);
    let cancel_token = cancel_token.clone();
    let running = Arc::new(AtomicBool::new(false));
    tokio::spawn(async move {
        let mut next_policy_sweep_at = 0i64;
        let mut last_disk_measurement_at = 0i64;
        loop {
            let tick_interval = watermark_check_interval(app_state.as_ref());
            tokio::select! {
                () = cancel_token.cancelled() => break,
                () = tokio::time::sleep(tick_interval) => {}
            }
            if !recording_enabled(app_state.as_ref()) {
                continue;
            }
            // Skip the tick entirely if the previous pass is still
            // deleting; passes must never overlap.
            let Some(_guard) = PassGuard::try_claim(&running) else {
                debug!("Retention supervisor tick skipped: previous pass still running");
                continue;
            };
            let now = now_ts();
            SupervisorHealth::stamp(&supervisor_health().retention_last_tick, now);

            let mut deleted = 0u64;
            if now >= next_policy_sweep_at {
                deleted += run_policy_sweep(&app_state, now).await;
                next_policy_sweep_at = now.saturating_add(policy_sweep_interval_secs(app_state.as_ref()));
            }
            if now.saturating_sub(last_disk_measurement_at) >= i64::try_from(MIN_DISK_PRESSURE_INTERVAL_SECS).unwrap_or(30) {
                last_disk_measurement_at = now;
                deleted += run_disk_pressure_sweep(&app_state).await;
            }
            if deleted > 0 {
                let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            }
        }
        debug!("Retention supervisor stopped");
    });
}

fn policy_sweep_interval_secs(app_state: &AppState) -> i64 {
    let secs = recording_config(app_state)
        .and_then(|cfg| cfg.retention.map(|retention| retention.sweep_interval_secs))
        .unwrap_or_else(shared::model::default_recording_retention_sweep_interval_secs);
    i64::try_from(secs.max(1)).unwrap_or(3600)
}

fn watermark_check_interval(app_state: &AppState) -> Duration {
    let secs = recording_config(app_state)
        .and_then(|cfg| cfg.disk.and_then(|disk| disk.cleanup_interval_secs))
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_WATERMARK_CHECK_INTERVAL_SECS);
    // Never tick faster than the disk measurement floor: a tighter
    // cadence would only burn wake-ups.
    Duration::from_secs(secs.clamp(1, 3600))
}

/// Age + count retention.
async fn run_policy_sweep(app_state: &Arc<AppState>, now: i64) -> u64 {
    let Some(config) = recording_config(app_state.as_ref()) else {
        return 0;
    };
    let Some(retention) = config.retention.as_ref() else {
        return 0;
    };
    let policy = super::super::recording_retention::RetentionConfig {
        keep_last_per_channel: retention.keep_last_per_channel,
        delete_after_days: retention.delete_after_days,
    };
    if policy.keep_last_per_channel.is_none() && policy.delete_after_days.is_none() {
        return 0;
    }
    let (_revision, tasks) = app_state.downloads.committed_snapshot().await;
    let candidates = super::super::recording_retention::compute_candidates(&tasks, &policy, now);
    if candidates.is_empty() {
        return 0;
    }
    let service = RecordingService::from_app_state(app_state);
    let claims = system_claims();
    let mut deleted = 0u64;
    for candidate in &candidates {
        match delete_for_retention(&service, &claims, &candidate.uuid).await {
            DeleteOutcome::Ok => {
                deleted += 1;
                info!(
                    target: "recording::audit",
                    "recording_retention_delete: reason={:?}", candidate.reason
                );
            }
            DeleteOutcome::Skipped | DeleteOutcome::Failed => {}
        }
    }
    if deleted > 0 {
        info!(
            "Retention policy sweep deleted {deleted} of {} candidate recording(s)",
            candidates.len()
        );
    }
    deleted
}

/// Free-space driven retention. Only runs when both watermarks are
/// configured and the recording root is measurable.
async fn run_disk_pressure_sweep(app_state: &Arc<AppState>) -> u64 {
    let Some(config) = recording_config(app_state.as_ref()) else {
        return 0;
    };
    let Some(disk) = config.disk.as_ref() else {
        return 0;
    };
    let disk_config = DiskConfig {
        high_water_percent: disk.high_water_percent,
        low_water_percent: disk.low_water_percent,
        safety_bytes: disk.safety_bytes,
    };
    if disk_config.high_water_percent.is_none() || disk_config.low_water_percent.is_none() {
        return 0;
    }
    let root = PathBuf::from(&config.directory);
    if root.as_os_str().is_empty() {
        return 0;
    }
    // Measure the recording root itself, never `storage_dir` or the
    // generic download directory — those can be on another filesystem.
    let Some((total_bytes, free_bytes)) = super::super::recording_disk::filesystem_capacity_for(&root) else {
        debug!("Disk-pressure sweep skipped: cannot measure {}", root.display());
        return 0;
    };
    if total_bytes == 0 {
        return 0;
    }
    let used = total_bytes.saturating_sub(free_bytes);
    let used_percent = u8::try_from(used.saturating_mul(100) / total_bytes).unwrap_or(100);

    let (_revision, tasks) = app_state.downloads.committed_snapshot().await;
    // The candidate ordering and the admission conditions stay in the
    // pure runner; only the delete side effect lives here, so the loop
    // can `await` instead of blocking a worker thread.
    let Some(candidates) = super::super::recording_worker_runner::disk_pressure_candidates(
        &tasks,
        &disk_config,
        used_percent,
        true,
    ) else {
        return 0;
    };
    let low = disk_config.low_water_percent.unwrap_or(0);
    let service = RecordingService::from_app_state(app_state);
    let claims = system_claims();
    let mut deleted = 0u64;
    let mut reclaimed = 0u64;
    for candidate in &candidates {
        if super::super::recording_worker_runner::pressure_relieved(total_bytes, free_bytes, reclaimed, low) {
            break;
        }
        let reclaimable = super::super::recording_worker_runner::reclaimable_bytes_for(&tasks, &candidate.uuid);
        if matches!(
            delete_for_retention(&service, &claims, &candidate.uuid).await,
            DeleteOutcome::Ok
        ) {
            deleted += 1;
            reclaimed = reclaimed.saturating_add(reclaimable);
        }
    }
    info!(
        target: "recording::audit",
        "recording_retention_delete: reason=watermark used_percent={used_percent} candidates={} deleted={deleted} reclaimed_bytes={reclaimed}",
        candidates.len()
    );
    deleted
}

async fn delete_for_retention(
    service: &RecordingService,
    claims: &Claims,
    uuid: &str,
) -> DeleteOutcome {
    match service.system_retention_delete(claims, uuid).await {
        Ok(()) => DeleteOutcome::Ok,
        // The task moved on (already deleted, no longer terminal, or not
        // safe to touch). Not an error: the next sweep re-evaluates.
        Err(ServiceError::UnknownRecording | ServiceError::InvalidState | ServiceError::Forbidden) => {
            DeleteOutcome::Skipped
        }
        Err(err) => {
            error!("Retention delete failed for recording {uuid}: {err}");
            DeleteOutcome::Failed
        }
    }
}
