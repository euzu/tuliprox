//! Runtime supervisors for the DVR.
//!
//! The recording feature ships three pure decision layers whose runners
//! were never started, so in production the DVR worked on the happy path
//! but could not bound its disk use or heal itself after a crash:
//!
//! - [`recording_retention`](super::recording_retention) computes the
//!   age/count candidates and
//!   [`recording_worker_runner`](super::recording_worker_runner) computes
//!   the disk-pressure candidates, but nothing called them.
//! - [`recording_reconciliation::reconcile`](super::recording_reconciliation::reconcile)
//!   decides how to repair queue/rule drift, but nothing called it, so a
//!   task left in `Deleting` by a crash stayed there forever.
//! - Lifecycle notifications were fired with a bare `tokio::spawn` from
//!   inside the download worker, so a transient provider error dropped
//!   the notification with no retry and no record.
//!
//! This module owns the runners. Each one is cancellation-aware, never
//! overlaps its own passes, and re-reads its configuration every tick so
//! a config reload takes effect without a restart.
//!
//! ## Layout
//!
//! The supervisor is split across five files to keep each focused:
//!
//! - [`health`] — last-tick timestamps and counters
//! - [`startup`] — crash-recovery reconciliation at boot
//! - [`retention`] — age / count / disk-pressure sweeps
//! - [`outbox`] — durable, per-channel notification retry
//! - [`mod`] (this file) — shared helpers, the entry point, and the
//!   cross-cutting tests

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::info;
use shared::model::{Claims, Permission, PermissionSet, CURRENT_PERMISSION_SCHEMA_VERSION, ROLE_ADMIN};
use tokio_util::sync::CancellationToken;

use crate::api::model::AppState;
use crate::model::RecordingConfig;

pub mod health;
pub mod outbox;
pub mod retention;
pub mod startup;

// Re-export the public surface so existing callers
// (`crate::api::model::recording::recording_supervisor::*`) keep
// working without an import change.
pub use health::{supervisor_health, SupervisorHealth};
pub use outbox::{notification_outbox, spawn_notification_outbox, NotificationOutbox};
pub use retention::spawn_retention_supervisor;
pub use startup::run_startup_reconciliation;

/// The effective recording configuration, cloned out of the `ArcSwap`
/// guard so no guard is held across an await.
pub(crate) fn recording_config(app_state: &AppState) -> Option<RecordingConfig> {
    app_state
        .app_config
        .config
        .load()
        .video
        .as_ref()
        .and_then(|video| video.download.as_ref())
        .and_then(|download| download.recording.clone())
}

/// `true` when the DVR is switched on.
///
/// The single predicate behind every `recording.enabled` gate — the REST
/// routes, the rule scheduler, the supervisors, and the WebSocket
/// filters. Keeping one definition is the point: four copies of
/// "is the DVR on?" would eventually disagree, and a half-disabled DVR
/// (routes refusing but the scheduler still materializing) is worse than
/// either state.
///
/// An absent `recording:` block means "use the defaults", and the default
/// is enabled.
pub fn recording_enabled(app_state: &AppState) -> bool {
    let config = app_state.app_config.config.load();
    config
        .video
        .as_ref()
        .and_then(|video| video.download.as_ref())
        .is_some_and(|download| download.recording.as_ref().is_none_or(|recording| recording.enabled))
}

pub(crate) fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Claims for a system-initiated action. The retention worker is not a
/// user; it holds the administrator role so it can act on shared and
/// legacy-owned recordings, and `RecordingWrite` so the service-level
/// permission checks pass.
pub(crate) fn system_claims() -> Claims {
    let mut permissions = PermissionSet::new();
    permissions.set(Permission::RecordingWrite);
    permissions.set(Permission::RecordingRead);
    let now = now_ts();
    Claims {
        username: crate::auth::SYSTEM_PRINCIPAL_USERNAME.to_string(),
        iss: "tuliprox".to_string(),
        iat: now,
        exp: now + 3600,
        roles: vec![ROLE_ADMIN.to_string()],
        permissions,
        pwd_version: 0,
        subject_id: Some(shared::model::UserId::builtin_admin()),
        permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
    }
}

/// A guard that makes a supervisor's passes strictly non-overlapping.
pub(crate) struct PassGuard(Arc<AtomicBool>);

impl PassGuard {
    pub(crate) fn try_claim(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(Arc::clone(flag)))
    }
}

impl Drop for PassGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Start every DVR supervisor. Called once the HTTP listener is bound so
/// the reconciliation pass cannot delay the bind.
pub async fn start_recording_supervisors(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    if !recording_enabled(app_state.as_ref()) {
        info!("Recording is disabled; DVR supervisors not started");
        return;
    }
    // Reconcile before anything else can materialize or sweep, so the
    // scheduler never plans against half-repaired state.
    run_startup_reconciliation(app_state).await;
    spawn_notification_outbox(app_state, cancel_token);
    spawn_retention_supervisor(app_state, cancel_token);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_download(recording: Option<shared::model::RecordingConfigDto>) -> crate::model::Config {
        crate::model::Config {
            video: Some(crate::model::VideoConfig::from(&shared::model::VideoConfigDto {
                download: Some(shared::model::VideoDownloadConfigDto { recording, ..Default::default() }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_absent_download_engine_means_disabled() {
        let config = crate::model::Config::default();
        let app_state = crate::api::model::create_test_app_state(config);
        assert!(!recording_enabled(app_state.as_ref()));
        assert!(recording_config(app_state.as_ref()).is_none());
    }

    #[tokio::test]
    async fn an_absent_recording_block_means_enabled() {
        let app_state = crate::api::model::create_test_app_state(config_with_download(None));
        assert!(recording_enabled(app_state.as_ref()));
    }

    #[tokio::test]
    async fn an_explicitly_disabled_recording_block_means_disabled() {
        let recording = shared::model::RecordingConfigDto { enabled: false, ..Default::default() };
        let app_state = crate::api::model::create_test_app_state(config_with_download(Some(recording)));
        assert!(!recording_enabled(app_state.as_ref()));
    }

    #[test]
    fn pass_guard_prevents_overlapping_passes() {
        let flag = Arc::new(AtomicBool::new(false));
        let first = PassGuard::try_claim(&flag).expect("first claim");
        assert!(PassGuard::try_claim(&flag).is_none());
        drop(first);
        assert!(PassGuard::try_claim(&flag).is_some());
    }
}
