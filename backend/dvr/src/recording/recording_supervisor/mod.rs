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

use super::recording_ctx::RecordingCtx;
use log::info;
use shared::model::{Claims, Permission, PermissionSet, CURRENT_PERMISSION_SCHEMA_VERSION, ROLE_ADMIN};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::{AppConfig, RecordingConfig};

pub mod health;
pub mod outbox;
pub mod retention;
pub mod startup;

// Re-export the public surface so existing callers
// (`crate::recording::recording_supervisor::*`) keep
// working without an import change.
pub use health::{supervisor_health, SupervisorHealth};
pub use outbox::{notification_outbox, spawn_notification_outbox, NotificationOutbox};
pub use retention::spawn_retention_supervisor;
pub use startup::run_startup_reconciliation;

/// The effective recording configuration, cloned out of the `ArcSwap`
/// guard so no guard is held across an await.
///
/// The DVR block lives at `Config.video.recording`.
pub fn recording_config(app_config: &AppConfig) -> Option<RecordingConfig> {
    app_config.config.load().recording().cloned()
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
pub fn recording_enabled(app_config: &AppConfig) -> bool {
    app_config.config.load().recording().is_some_and(|recording| recording.enabled)
}

pub fn now_ts() -> i64 { chrono::Utc::now().timestamp() }

/// Claims for a system-initiated action. The retention worker is not a
/// user; it holds the administrator role so it can act on shared and
/// legacy-owned recordings, and the recording permissions the
/// service-level checks require.
pub fn system_claims() -> Claims {
    let mut permissions = PermissionSet::new();
    permissions.set(Permission::RecordingCreate);
    permissions.set(Permission::RecordingManage);
    permissions.set(Permission::RecordingDelete);
    permissions.set(Permission::RecordingRead);
    let now = now_ts();
    Claims {
        username: tuliprox_auth::SYSTEM_PRINCIPAL_USERNAME.to_string(),
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
pub struct PassGuard(Arc<AtomicBool>);

impl PassGuard {
    pub fn try_claim(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok().map(|_| Self(Arc::clone(flag)))
    }
}

impl Drop for PassGuard {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

/// Start every DVR supervisor. Called once the HTTP listener is bound so
/// the reconciliation pass cannot delay the bind.
pub async fn start_recording_supervisors(ctx: &RecordingCtx, cancel_token: &CancellationToken) {
    if !recording_enabled(&ctx.app_config) {
        info!("Recording is disabled; DVR supervisors not started");
        return;
    }
    // Reconcile before anything else can materialize or sweep, so the
    // scheduler never plans against half-repaired state.
    run_startup_reconciliation(ctx).await;
    spawn_notification_outbox(ctx, cancel_token);
    spawn_retention_supervisor(ctx, cancel_token);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_download(recording: Option<shared::model::RecordingConfigDto>) -> tuliprox_core::model::Config {
        let canonical_dto = recording.unwrap_or_default();
        let canonical = tuliprox_core::model::RecordingConfig::from(&canonical_dto);
        tuliprox_core::model::Config {
            video: Some(tuliprox_core::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(canonical),
            }),
            ..Default::default()
        }
    }

    /// An `AppConfig` carrying just the config under test. The predicates below
    /// read nothing else.
    fn test_app_config(config: tuliprox_core::model::Config) -> tuliprox_core::model::AppConfig {
        tuliprox_core::model::AppConfig {
            config: Arc::new(arc_swap::ArcSwap::from_pointee(config)),
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

    #[tokio::test]
    async fn an_absent_download_engine_means_disabled() {
        let app_config = test_app_config(tuliprox_core::model::Config::default());
        assert!(!recording_enabled(&app_config));
        assert!(recording_config(&app_config).is_none());
    }

    #[tokio::test]
    async fn an_absent_recording_block_means_enabled() {
        let app_config = test_app_config(config_with_download(None));
        assert!(recording_enabled(&app_config));
    }

    #[tokio::test]
    async fn an_explicitly_disabled_recording_block_means_disabled() {
        let recording = shared::model::RecordingConfigDto { enabled: false, ..Default::default() };
        let app_config = test_app_config(config_with_download(Some(recording)));
        assert!(!recording_enabled(&app_config));
    }

    #[tokio::test]
    async fn hot_reload_swaps_canonical_recording_through_the_arcswap_accessor() {
        // `recording_config()` reads `Config.video.recording`, and a hot reload that swaps the
        // `ArcSwap<Config>` makes the new recording visible to every
        // supervisor.
        let make_recording = |dir: &str| {
            tuliprox_core::model::RecordingConfig::from(&shared::model::RecordingConfigDto {
                enabled: true,
                directory: Some(dir.to_string()),
                ..shared::model::RecordingConfigDto::default()
            })
        };
        let initial = tuliprox_core::model::Config {
            video: Some(tuliprox_core::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(make_recording("/initial/recordings")),
            }),
            ..tuliprox_core::model::Config::default()
        };
        let app_config = test_app_config(initial.clone());

        // Before reload: canonical recording visible.
        let before = recording_config(&app_config).expect("video recording visible");
        assert_eq!(before.directory, "/initial/recordings");
        assert!(recording_enabled(&app_config));

        // Hot reload: swap to a new Config with a different recording directory.
        let mut next = initial;
        next.video.as_mut().and_then(|video| video.recording.as_mut()).expect("initial recording set").directory =
            "/reloaded/recordings".to_string();
        app_config.config.store(std::sync::Arc::new(next));

        let after = recording_config(&app_config).expect("recording still visible after reload");
        assert_eq!(after.directory, "/reloaded/recordings", "recording_config must read the swapped Config.video");
    }

    #[tokio::test]
    async fn recording_enabled_predicate_uses_canonical_recording_block() {
        // `recording_enabled()` must consult `Config.video.recording.enabled`.
        let enabled_cfg = tuliprox_core::model::Config {
            video: Some(tuliprox_core::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(tuliprox_core::model::RecordingConfig::from(&shared::model::RecordingConfigDto {
                    enabled: true,
                    ..Default::default()
                })),
            }),
            ..tuliprox_core::model::Config::default()
        };
        let app_enabled = test_app_config(enabled_cfg);
        assert!(recording_enabled(&app_enabled), "enabled canonical block must enable DVR");

        let disabled_cfg = tuliprox_core::model::Config {
            video: Some(tuliprox_core::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(tuliprox_core::model::RecordingConfig::from(&shared::model::RecordingConfigDto {
                    enabled: false,
                    ..Default::default()
                })),
            }),
            ..tuliprox_core::model::Config::default()
        };
        let app_disabled = test_app_config(disabled_cfg);
        assert!(!recording_enabled(&app_disabled), "disabled canonical block must disable DVR");
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
