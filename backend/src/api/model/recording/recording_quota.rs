//! Conservative quota ledger.
//!
//! Quota usage is **derived** from the committed task metadata
//! inside the queue — no second persisted quota database. Each
//! task contributes a charge based on its state.
//! Two pools: per-user private (keyed by immutable `UserId`) and
//! one shared pool.
//!
//! Regular users see their own private totals plus coarse shared
//! availability. Administrators see shared totals without automatic
//! per-user private detail.
//!
//! Admission goes through `would_exceed`: a proposed `delta` for
//! a given pool is compared against the configured limit, and
//! the outcome is one of `Ok`, `OverLimit { ... }`, or `Unlimited`.
//! The recording service calls it under the queue mutation boundary
//! before persisting a new task.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use shared::model::recording::RecordingMetadata;
use shared::model::UserId;

use crate::api::model::download::{DownloadState, PersistedFileDownload};
use crate::api::model::FileDownload;

/// Pool a task belongs to. Private is keyed by the immutable
/// `UserId`; shared is the single shared pool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QuotaPool {
    Private(UserId),
    Shared,
}

/// Result of asking "if I add `delta` bytes to this pool, would I
/// exceed the configured limit?". Admission prevents further
/// admission when over limit; we surface the
/// `OverLimit { limit, used, would_be }` details so the HTTP layer
/// can return a stable `recording_quota_exceeded` code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionOutcome {
    Ok { used_after: u64, limit: u64 },
    OverLimit { used: u64, limit: u64, would_be: u64 },
    /// `limit` is `None` for this pool.
    Unlimited { used_after: u64 },
}

/// Effective per-pool totals. The integer is the total charged
/// bytes for every task in that pool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuotaTotals {
    pub private: HashMap<UserId, u64>,
    pub shared: u64,
}

/// Configured limits, derived from `RecordingQuotaConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct QuotaLimits {
    pub default_private_bytes: Option<u64>,
    pub per_user_bytes: HashMap<UserId, u64>,
    pub shared_bytes: Option<u64>,
}

/// Effective limit for a given pool. `None` means unlimited.
pub fn limit_for_pool(pool: &QuotaPool, limits: &QuotaLimits) -> Option<u64> {
    match pool {
        QuotaPool::Private(uid) => limits
            .per_user_bytes
            .get(uid)
            .copied()
            .or(limits.default_private_bytes),
        QuotaPool::Shared => limits.shared_bytes,
    }
}

/// Admission check: would adding `delta` bytes to `pool` exceed
/// the configured limit? `used` is the pool's current total from
/// `compute_totals`.
pub fn would_exceed(
    pool: &QuotaPool,
    used: u64,
    delta: u64,
    limits: &QuotaLimits,
) -> AdmissionOutcome {
    let Some(limit) = limit_for_pool(pool, limits) else {
        return AdmissionOutcome::Unlimited { used_after: used.saturating_add(delta) };
    };
    let would_be = used.saturating_add(delta);
    if would_be > limit {
        AdmissionOutcome::OverLimit { used, limit, would_be }
    } else {
        AdmissionOutcome::Ok { used_after: would_be, limit }
    }
}

/// Estimate the reservation for a future recording.
///
/// - If `bitrate_bytes_per_sec > 0`, use `remaining_secs × bitrate`.
/// - Otherwise, use `remaining_secs × fallback_bytes_per_minute / 60`
///   and return `UnknownBitrate`.
pub fn estimate_reservation(
    remaining_secs: u64,
    bitrate_bytes_per_sec: u64,
    fallback_bytes_per_minute: u64,
) -> (u64, ReservationWarning) {
    if bitrate_bytes_per_sec > 0 {
        let bytes = remaining_secs.saturating_mul(bitrate_bytes_per_sec);
        (bytes, ReservationWarning::None)
    } else {
        // Round up to the next whole minute to avoid under-reserving.
        let minutes = remaining_secs.div_ceil(60).max(1);
        let bytes = minutes.saturating_mul(fallback_bytes_per_minute);
        (bytes, ReservationWarning::UnknownBitrate)
    }
}

/// Why the caller should emit a warning. The HTTP layer maps
/// `UnknownBitrate` to the `recording_unknown_bitrate` wire code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationWarning {
    None,
    UnknownBitrate,
}

/// DTO returned to a regular user. Includes the user's own private
/// totals and a coarse shared availability summary — never other
/// users' private totals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaDto {
    pub private: PrivateQuotaDto,
    pub shared: SharedAvailabilityDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrivateQuotaDto {
    pub user_id: UserId,
    pub measured_bytes: u64,
    pub reserved_bytes: u64,
    pub limit_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedAvailabilityDto {
    pub used_bytes: u64,
    pub limit_bytes: Option<u64>,
}

/// Read-only view of a recording task for quota computation.
/// Lets the pure helpers be unit-tested without a real
/// `FileDownload` (which has many unrelated fields and a required
/// `reqwest::Url`).
pub trait QuotaRecordingTaskView {
    fn state(&self) -> &DownloadState;
    fn recording(&self) -> Option<&RecordingMetadata>;
    /// Stable task identifier. Returns `""` for views that do
    /// not expose one; callers (e.g. retention) require a
    /// non-empty uuid to be useful.
    fn uuid(&self) -> &str;
}

impl QuotaRecordingTaskView for FileDownload {
    fn state(&self) -> &DownloadState {
        &self.state
    }
    fn recording(&self) -> Option<&RecordingMetadata> {
        self.recording.as_ref()
    }
    fn uuid(&self) -> &str {
        &self.uuid
    }
}

impl QuotaRecordingTaskView for PersistedFileDownload {
    fn state(&self) -> &DownloadState {
        &self.state
    }
    fn recording(&self) -> Option<&RecordingMetadata> {
        self.recording.as_ref()
    }
    fn uuid(&self) -> &str {
        &self.uuid
    }
}

/// `charge_for_task` is the public surface that walks a real
/// `FileDownload`. The shape-based test below covers the same
/// state→charge logic; the `charge_for_task` wrapper is a
/// trivial match on `recording.is_none()` and `charge_for_state`
/// so it is not exercised separately here.
pub fn charge_for_task<V: QuotaRecordingTaskView>(task: &V) -> u64 {
    match task.recording() {
        None => 0,
        Some(meta) => charge_for_state(task.state(), meta),
    }
}

/// Pure state-driven charge. Kept separate from `charge_for_task`
/// so it can be unit-tested without a full `FileDownload`.
///
pub fn charge_for_state(state: &DownloadState, meta: &RecordingMetadata) -> u64 {
    match state {
        DownloadState::Scheduled
        | DownloadState::Queued
        | DownloadState::WaitingForCapacity
        | DownloadState::RetryWaiting
        | DownloadState::Paused => meta.reserved_bytes,
        DownloadState::Downloading => meta.reserved_bytes.max(meta.measured_bytes),
        DownloadState::Completed => meta.measured_bytes,
        DownloadState::Failed | DownloadState::Cancelled => {
            meta.measured_bytes
        }
    }
}

/// Pool the given task belongs to. Returns `None` for non-recording
/// tasks (so generic downloads are not charged).
pub fn quota_pool_for_task<V: QuotaRecordingTaskView>(task: &V) -> Option<QuotaPool> {
    let meta = task.recording()?;
    Some(match (&meta.visibility, &meta.owner) {
        (shared::model::recording::RecordingVisibility::Shared, _)
        | (_, shared::model::recording::RecordingOwner::LegacyAdmin) => QuotaPool::Shared,
        (_, shared::model::recording::RecordingOwner::User(uid)) => QuotaPool::Private(uid.clone()),
    })
}

/// Build a `QuotaLedger` from a set of tasks. `tasks` should be the
/// full set of recording tasks in the queue; the ledger sums each
/// task's per-state charge into the right pool.
pub fn compute_totals<V: QuotaRecordingTaskView>(tasks: &[V]) -> QuotaTotals {
    let mut totals = QuotaTotals::default();
    for task in tasks {
        let Some(pool) = quota_pool_for_task(task) else {
            continue;
        };
        let charge = charge_for_task(task);
        match pool {
            QuotaPool::Private(uid) => *totals.private.entry(uid).or_insert(0) += charge,
            QuotaPool::Shared => totals.shared += charge,
        }
    }
    totals
}

/// Sum the charge for a single pool over borrowed tasks.
///
/// `compute_totals` allocates a `HashMap<UserId, u64>` covering every
/// pool in the queue; admission checks read exactly one entry out of it
/// and run inside the queue mutation boundary, so the map and the
/// clones it implies are pure waste there. This is the fast path: one
/// pass, no allocation, borrowed input.
pub fn used_bytes_in_pool<'a, V, I>(tasks: I, pool: &QuotaPool) -> u64
where
    V: QuotaRecordingTaskView + 'a,
    I: IntoIterator<Item = &'a V>,
{
    let mut total = 0u64;
    for task in tasks {
        let Some(task_pool) = quota_pool_for_task(task) else {
            continue;
        };
        if &task_pool == pool {
            total = total.saturating_add(charge_for_task(task));
        }
    }
    total
}

/// Build the regular-user DTO. `subject_id` is the user asking.
/// Other users' private totals are never included.
pub fn regular_user_dto<V: QuotaRecordingTaskView>(
    subject_id: &UserId,
    totals: &QuotaTotals,
    limits: &QuotaLimits,
    tasks: &[V],
) -> QuotaDto {
    let (measured, reserved) = split_measured_reserved_for_user_from_tasks(subject_id, tasks);
    let limit = limit_for_pool(&QuotaPool::Private(subject_id.clone()), limits);
    QuotaDto {
        private: PrivateQuotaDto {
            user_id: subject_id.clone(),
            measured_bytes: measured,
            reserved_bytes: reserved,
            limit_bytes: limit,
        },
        shared: SharedAvailabilityDto {
            used_bytes: totals.shared,
            limit_bytes: limits.shared_bytes,
        },
    }
}

/// Split a user's private total into measured vs reserved by
/// walking the tasks. Used by `recording_service` to produce the
/// DTO without storing a second database.
pub fn split_measured_reserved_for_user_from_tasks<V: QuotaRecordingTaskView>(
    subject_id: &UserId,
    tasks: &[V],
) -> (u64, u64) {
    let mut measured = 0u64;
    let mut reserved = 0u64;
    for task in tasks {
        let Some(meta) = task.recording() else {
            continue;
        };
        let is_user = meta.visibility == shared::model::recording::RecordingVisibility::Private && matches!(
            &meta.owner,
            shared::model::recording::RecordingOwner::User(uid) if uid == subject_id
        );
        if !is_user {
            continue;
        }
        let charge = charge_for_task(task);
        // The reservation is the part of the charge that comes
        // from `reserved_bytes`; the measured part is everything
        // over that, capped at the total charge.
        let r = meta.reserved_bytes;
        let m = charge.saturating_sub(r);
        reserved = reserved.saturating_add(r);
        measured = measured.saturating_add(m);
    }
    (measured, reserved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::recording::{
        RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility,
    };

    fn make_meta(owner: RecordingOwner, reserved: u64, measured: u64) -> RecordingMetadata {
        RecordingMetadata {
            owner,
            visibility: RecordingVisibility::Private,
            source: Some(RecordingSource::new("t1", "v1", "in1")),
            program_start: None,
            program_end: None,
            scheduled_start: None,
            scheduled_end: None,
            pre_roll_secs: 0,
            post_roll_secs: 0,
            channel_id: None,
            channel_name: None,
            program_title: None,
            epg: None,
            provenance: shared::model::recording::RecordingProvenance::default(),
            relative_path: None,
            partial_relative_path: None,
            reserved_bytes: reserved,
            measured_bytes: measured,
            completed_at: None,
            notification_markers: Vec::new(),
            deleting_previous_state: None,
        }
    }

    // Lightweight stand-in for `FileDownload` so tests of the
    // pure `charge_for_state` and pool-resolution helpers don't
    // need the full HTTP/URL machinery. The real
    // `charge_for_task` reads only `state` and `recording`; the
    // `TaskShape` mirrors that.
    struct TaskShape {
        state: DownloadState,
        recording: Option<RecordingMetadata>,
    }

    impl QuotaRecordingTaskView for TaskShape {
        fn state(&self) -> &DownloadState {
            &self.state
        }
        fn recording(&self) -> Option<&RecordingMetadata> {
            self.recording.as_ref()
        }
        fn uuid(&self) -> &'static str {
            // The shape fixture has no uuid field; tests that
            // depend on the uuid path (retention) use a richer
            // fixture in their own module.
            ""
        }
    }

    fn task(
        owner: RecordingOwner,
        state: DownloadState,
        reserved: u64,
        measured: u64,
    ) -> TaskShape {
        TaskShape {
            state,
            recording: Some(make_meta(owner, reserved, measured)),
        }
    }

    // Mirror the `charge_for_task` body against `TaskShape` so the
    // tests can exercise the function without a real FileDownload.
    fn charge_task_shape(t: &TaskShape) -> u64 {
        charge_for_task(t)
    }

    fn pool_for_shape(t: &TaskShape) -> Option<QuotaPool> {
        quota_pool_for_task(t)
    }

    #[test]
    fn charge_scheduled_is_reservation() {
        let t = task(
            RecordingOwner::User(UserId::from("web:alice")),
            DownloadState::Scheduled,
            1000,
            0,
        );
        assert_eq!(charge_task_shape(&t), 1000);
    }

    #[test]
    fn charge_queued_is_reservation() {
        let t = task(
            RecordingOwner::User(UserId::from("web:alice")),
            DownloadState::Queued,
            2000,
            0,
        );
        assert_eq!(charge_task_shape(&t), 2000);
    }

    #[test]
    fn charge_waiting_is_reservation() {
        for state in &[
            DownloadState::WaitingForCapacity,
            DownloadState::RetryWaiting,
            DownloadState::Paused,
        ] {
            let t = task(
                RecordingOwner::User(UserId::from("web:alice")),
                state.clone(),
                500,
                0,
            );
            assert_eq!(charge_task_shape(&t), 500, "state {state:?}");
        }
    }

    #[test]
    fn charge_downloading_is_max_of_reservation_and_measured() {
        // measured > reserved → measured
        let t = task(
            RecordingOwner::User(UserId::from("web:alice")),
            DownloadState::Downloading,
            1000,
            1500,
        );
        assert_eq!(charge_task_shape(&t), 1500);
        // reserved > measured → reserved
        let t = task(
            RecordingOwner::User(UserId::from("web:alice")),
            DownloadState::Downloading,
            2000,
            100,
        );
        assert_eq!(charge_task_shape(&t), 2000);
    }

    #[test]
    fn charge_completed_is_measured() {
        let t = task(
            RecordingOwner::User(UserId::from("web:alice")),
            DownloadState::Completed,
            0,
            3000,
        );
        assert_eq!(charge_task_shape(&t), 3000);
    }

    #[test]
    fn charge_failed_cancelled_is_partial_measured() {
        // No partial file → 0
        let t = task(
            RecordingOwner::User(UserId::from("web:alice")),
            DownloadState::Failed,
            5000,
            0,
        );
        assert_eq!(charge_task_shape(&t), 0);
        // Partial file present → measured
        let t = task(
            RecordingOwner::User(UserId::from("web:alice")),
            DownloadState::Cancelled,
            5000,
            200,
        );
        assert_eq!(charge_task_shape(&t), 200);
    }

    #[test]
    fn charge_is_zero_for_non_recording_task() {
        let t = TaskShape {
            state: DownloadState::Completed,
            recording: None,
        };
        assert_eq!(charge_task_shape(&t), 0);
    }

    #[test]
    fn private_pool_for_user_owner() {
        let t = task(
            RecordingOwner::User(UserId::from("web:alice")),
            DownloadState::Scheduled,
            100,
            0,
        );
        assert_eq!(
            pool_for_shape(&t),
            Some(QuotaPool::Private(UserId::from("web:alice")))
        );
    }

    #[test]
    fn shared_pool_for_legacy_admin() {
        let t = task(
            RecordingOwner::LegacyAdmin,
            DownloadState::Scheduled,
            100,
            0,
        );
        assert_eq!(pool_for_shape(&t), Some(QuotaPool::Shared));
    }

    // `charge_for_task` is the public surface that walks a real
    // `FileDownload`. The shape-based test below covers the same
    // state→charge logic; the `charge_for_task` wrapper is a
    // trivial match on `recording.is_none()` and `charge_for_state`
    // so it is not exercised separately here.

    #[test]
    fn admission_ok_under_limit() {
        let limits = QuotaLimits {
            default_private_bytes: Some(1000),
            ..Default::default()
        };
        let out = would_exceed(&QuotaPool::Private(UserId::from("web:alice")), 600, 300, &limits);
        assert_eq!(out, AdmissionOutcome::Ok { used_after: 900, limit: 1000 });
    }

    #[test]
    fn admission_over_limit() {
        let limits = QuotaLimits {
            default_private_bytes: Some(1000),
            ..Default::default()
        };
        let out = would_exceed(&QuotaPool::Private(UserId::from("web:alice")), 800, 300, &limits);
        assert_eq!(
            out,
            AdmissionOutcome::OverLimit { used: 800, limit: 1000, would_be: 1100 }
        );
    }

    #[test]
    fn admission_unlimited_when_no_limit() {
        let limits = QuotaLimits::default();
        let out = would_exceed(&QuotaPool::Private(UserId::from("web:alice")), 999_999, 1, &limits);
        assert_eq!(out, AdmissionOutcome::Unlimited { used_after: 1_000_000 });
    }

    #[test]
    fn per_user_override_beats_default() {
        let mut limits = QuotaLimits {
            default_private_bytes: Some(1000),
            ..Default::default()
        };
        limits
            .per_user_bytes
            .insert(UserId::from("web:alice"), 5_000);
        assert_eq!(
            limit_for_pool(&QuotaPool::Private(UserId::from("web:alice")), &limits),
            Some(5_000)
        );
        // bob falls back to the default
        assert_eq!(
            limit_for_pool(&QuotaPool::Private(UserId::from("web:bob")), &limits),
            Some(1000)
        );
    }

    #[test]
    fn estimate_reservation_known_bitrate() {
        let (bytes, warn) = estimate_reservation(3600, 500_000, 0);
        assert_eq!(bytes, 3600 * 500_000);
        assert_eq!(warn, ReservationWarning::None);
    }

    #[test]
    fn estimate_reservation_unknown_bitrate_uses_fallback() {
        // 90 minutes with 8 MiB/min fallback → 90 * 8 MiB
        let (bytes, warn) = estimate_reservation(90 * 60, 0, 8 * 1024 * 1024);
        assert_eq!(bytes, 90 * 8 * 1024 * 1024);
        assert_eq!(warn, ReservationWarning::UnknownBitrate);
    }

    #[test]
    fn estimate_reservation_unknown_bitrate_rounds_up_to_minute() {
        // 30 seconds → rounds up to 1 minute
        let (bytes, warn) = estimate_reservation(30, 0, 8 * 1024 * 1024);
        assert_eq!(bytes, 8 * 1024 * 1024);
        assert_eq!(warn, ReservationWarning::UnknownBitrate);
    }

    #[test]
    fn estimate_reservation_unknown_bitrate_clamps_zero_to_minute() {
        // 0 seconds → at least 1 minute (sanity)
        let (bytes, warn) = estimate_reservation(0, 0, 8 * 1024 * 1024);
        assert_eq!(bytes, 8 * 1024 * 1024);
        assert_eq!(warn, ReservationWarning::UnknownBitrate);
    }

    #[test]
    fn regular_user_dto_redacts_other_users() {
        let mut totals = QuotaTotals::default();
        totals.private.insert(UserId::from("web:alice"), 6000);
        totals.private.insert(UserId::from("web:bob"), 9999);
        totals.shared = 200;
        let limits = QuotaLimits {
            default_private_bytes: Some(10_000),
            shared_bytes: Some(50_000),
            ..Default::default()
        };
        let tasks = vec![
            task(
                RecordingOwner::User(UserId::from("web:alice")),
                DownloadState::Completed,
                0,
                6000,
            ),
            task(
                RecordingOwner::User(UserId::from("web:bob")),
                DownloadState::Scheduled,
                9999,
                0,
            ),
        ];
        let dto = regular_user_dto(&UserId::from("web:alice"), &totals, &limits, &tasks);
        // Own totals present
        assert_eq!(dto.private.measured_bytes, 6000);
        assert_eq!(dto.private.reserved_bytes, 0);
        assert_eq!(dto.private.limit_bytes, Some(10_000));
        // Shared availability present (coarse)
        assert_eq!(dto.shared.used_bytes, 200);
        assert_eq!(dto.shared.limit_bytes, Some(50_000));
        // bob's 9999 is not in the DTO
        let json = serde_json::to_value(&dto).unwrap();
        let s = serde_json::to_string(&json).unwrap();
        assert!(!s.contains("9999"), "DTO must not leak other users' totals: {s}");
    }

}
