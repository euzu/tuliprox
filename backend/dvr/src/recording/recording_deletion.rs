//! Two-phase recording deletion and startup recovery.
//!
//! Deletion is split into three steps so the persisted queue never
//! observes an in-memory state without a corresponding file on disk:
//!
//! 1. `begin_deletion` runs inside the queue mutation boundary; it
//!    stamps the recording's `deleting_previous_state` and the task's
//!    `state` so the candidate commits `Deleting` atomically.
//! 2. `execute_deletion` runs **after** the boundary is released; it
//!    calls `safe_unlink` on the recorded file. Missing files count as
//!    success.
//! 3. `finalize_deletion` runs inside a new boundary; it removes the
//!    task from the queue, releases the quota charge, and clears
//!    `deleting_previous_state`.
//!
//! Startup recovery normalizes any leftover `Deleting` task whose
//! physical file is gone (finalize), whose file is still present
//! (restore previous state), or whose path is unsafe (restore +
//! security log).

use crate::download::{
    DownloadQueue, DownloadState, FileDownload, PersistedDownloadQueue, PersistedFileDownload, QueueMutationError,
};
use shared::model::{DeletionPreviousState, RecordingMetadata};
use std::path::{Path, PathBuf};
use tuliprox_core::utils::{no_follow_existing, safe_unlink};

/// Errors that can occur during the three phases.
#[derive(Debug, thiserror::Error)]
pub enum DeletionError {
    /// The UUID did not match any task in the queue.
    #[error("recording not found")]
    UnknownTask,
    /// The matched task is not a recording.
    #[error("task is not a recording")]
    NotARecording,
    /// The matched task is not in a terminal state, so deletion cannot
    /// begin.
    #[error("recording is not in a terminal state")]
    NotTerminal,
    /// The caller is not permitted to delete this recording. Reported
    /// from inside the mutation boundary so authorization and the state
    /// transition observe the same task.
    #[error("recording deletion forbidden")]
    Forbidden,
    /// Marking the recording as `Deleting` failed.
    #[error("begin deletion failed: {0}")]
    BeginFailed(QueueMutationError),
    /// File deletion failed in a way that is not safe to
    /// ignore.
    #[error("physical delete failed: {0}")]
    DeleteFailed(std::io::Error),
    /// Removing the task from the queue failed.
    #[error("finalize deletion failed: {0}")]
    FinalizeFailed(QueueMutationError),
}

/// Locate a recording task in the candidate by uuid. Returns
/// `(bucket, index)` where `bucket` is one of `"queue"`, `"scheduled"`,
/// `"active"`, `"finished"`. Returns `None` if the uuid is not in the
/// candidate.
fn locate(candidate: &PersistedDownloadQueue, uuid: &str) -> Option<(&'static str, usize)> {
    if let Some(idx) = candidate.queue.iter().position(|d| d.uuid == uuid) {
        return Some(("queue", idx));
    }
    if let Some(idx) = candidate.scheduled.iter().position(|d| d.uuid == uuid) {
        return Some(("scheduled", idx));
    }
    if candidate.active.as_ref().is_some_and(|d| d.uuid == uuid) {
        return Some(("active", 0));
    }
    if let Some(idx) = candidate.finished.iter().position(|d| d.uuid == uuid) {
        return Some(("finished", idx));
    }
    None
}

/// Read the recording metadata from a candidate. Returns `None` if the
/// uuid is not a recording or has no metadata.
fn read_meta(candidate: &PersistedDownloadQueue, uuid: &str) -> Option<RecordingMetadata> {
    if let Some(d) = candidate.queue.iter().find(|d| d.uuid == uuid) {
        return d.recording.clone();
    }
    if let Some(d) = candidate.scheduled.iter().find(|d| d.uuid == uuid) {
        return d.recording.clone();
    }
    if let Some(d) = candidate.active.as_ref() {
        if d.uuid == uuid {
            return d.recording.clone();
        }
    }
    if let Some(d) = candidate.finished.iter().find(|d| d.uuid == uuid) {
        return d.recording.clone();
    }
    None
}

/// Derive the prior terminal state from a recording's current state.
/// Returns `None` when the state is not a terminal one (and therefore
/// deletion cannot begin).
#[cfg(test)]
fn prior_terminal_state(download: &FileDownload) -> Option<DeletionPreviousState> {
    match download.state {
        DownloadState::Completed => Some(DeletionPreviousState::Completed),
        DownloadState::Failed => Some(DeletionPreviousState::Failed),
        DownloadState::Cancelled => Some(DeletionPreviousState::Cancelled),
        _ => None,
    }
}

/// Everything the out-of-boundary unlink step needs, captured by the
/// same `mutate` that stamped the task. Carrying it forward removes the
/// second `lookup_recording` the caller used to perform, and with it the
/// window in which the two lookups could disagree.
#[derive(Debug, Clone)]
pub struct DeletionTarget {
    pub uuid: String,
    pub file_path: PathBuf,
    pub previous_state: DeletionPreviousState,
}

impl DeletionTarget {
    /// The single file this deletion owns. `Completed` recordings own
    /// their final file; `Failed` / `Cancelled` ones never reached
    /// finalization, so they own the `.partial`.
    pub fn path_to_unlink(&self) -> PathBuf {
        match self.previous_state {
            DeletionPreviousState::Completed => self.file_path.clone(),
            DeletionPreviousState::Failed | DeletionPreviousState::Cancelled => {
                crate::recording_worker::recording_partial_path(&self.file_path)
            }
        }
    }
}

/// Mark the recording as `Deleting` under the queue mutation boundary,
/// with `permit` deciding — inside that same boundary — whether the
/// caller may do so. The candidate is persisted atomically with the new
/// `deleting_previous_state`; on success the in-memory queue reflects
/// `Deleting` and the on-disk file is unchanged.
pub async fn begin_deletion_authorized<F>(
    queue: &DownloadQueue,
    uuid: &str,
    permit: F,
) -> Result<DeletionTarget, DeletionError>
where
    F: FnOnce(&RecordingMetadata) -> bool,
{
    crate::download::mutate(queue, |candidate| {
        let Some(meta) = read_meta(candidate, uuid) else {
            return Err(QueueMutationError::UnknownRecording);
        };
        if !permit(&meta) {
            return Err(QueueMutationError::Forbidden);
        }
        let Some((bucket, idx)) = locate(candidate, uuid) else {
            return Err(QueueMutationError::UnknownRecording);
        };
        // Resolve the current persisted file to read its terminal state.
        let task = match bucket {
            "queue" => &candidate.queue[idx],
            "scheduled" => &candidate.scheduled[idx],
            "active" => {
                let Some(active) = candidate.active.as_ref() else {
                    return Err(QueueMutationError::UnknownRecording);
                };
                active
            }
            "finished" => &candidate.finished[idx],
            _ => return Err(QueueMutationError::UnknownRecording),
        };
        let Some(prior) = prior_terminal_state_runtime(task) else {
            return Err(QueueMutationError::NotInTerminalState);
        };
        // Stamp the deletion. The persisted task gets `state = Cancelled`
        // (the canonical "removing" marker) plus
        // `recording.deleting_previous_state = Some(prior)` so startup
        // recovery can restore the prior state if the file is still
        // present. Measured/reserved bytes are kept as-is so quota
        // accounting survives a failed or interrupted deletion; they are
        // released when the task is removed in `finalize_deletion`.
        let target =
            DeletionTarget { uuid: uuid.to_string(), file_path: task.file_path.clone(), previous_state: prior };
        let mut new_meta = meta;
        new_meta.deleting_previous_state = Some(prior);
        apply_meta(candidate, bucket, idx, new_meta);
        set_task_state(candidate, bucket, idx, DownloadState::Cancelled);
        Ok(target)
    })
    .await
    .map_err(|err| match err {
        QueueMutationError::Forbidden => DeletionError::Forbidden,
        QueueMutationError::NotInTerminalState => DeletionError::NotTerminal,
        QueueMutationError::UnknownRecording => DeletionError::UnknownTask,
        other => DeletionError::BeginFailed(other),
    })
}

/// Unconditional variant, kept for callers that have already
/// authorized (and for the unit tests, which exercise the state
/// machine rather than the policy).
pub async fn begin_deletion(queue: &DownloadQueue, uuid: &str) -> Result<DeletionTarget, DeletionError> {
    begin_deletion_authorized(queue, uuid, |_| true).await
}

fn prior_terminal_state_runtime(download: &PersistedFileDownload) -> Option<DeletionPreviousState> {
    match download.state {
        DownloadState::Completed => Some(DeletionPreviousState::Completed),
        DownloadState::Failed => Some(DeletionPreviousState::Failed),
        DownloadState::Cancelled => Some(DeletionPreviousState::Cancelled),
        _ => None,
    }
}

fn apply_meta(candidate: &mut PersistedDownloadQueue, bucket: &'static str, idx: usize, meta: RecordingMetadata) {
    match bucket {
        "queue" => {
            if let Some(d) = candidate.queue.get_mut(idx) {
                d.recording = Some(meta);
            }
        }
        "scheduled" => {
            if let Some(d) = candidate.scheduled.get_mut(idx) {
                d.recording = Some(meta);
            }
        }
        "active" => {
            if let Some(d) = candidate.active.as_mut() {
                d.recording = Some(meta);
            }
        }
        "finished" => {
            if let Some(d) = candidate.finished.get_mut(idx) {
                d.recording = Some(meta);
            }
        }
        _ => unreachable!(),
    }
}

/// Set the persisted `state` on a task in the given bucket. Companion to
/// `apply_meta` — the deletion transition needs both the recording
/// metadata flag (`deleting_previous_state`) and the canonical task
/// state (`Cancelled`) to land atomically.
fn set_task_state(candidate: &mut PersistedDownloadQueue, bucket: &'static str, idx: usize, state: DownloadState) {
    match bucket {
        "queue" => {
            if let Some(d) = candidate.queue.get_mut(idx) {
                d.state = state;
            }
        }
        "scheduled" => {
            if let Some(d) = candidate.scheduled.get_mut(idx) {
                d.state = state;
            }
        }
        "active" => {
            if let Some(d) = candidate.active.as_mut() {
                d.state = state;
            }
        }
        "finished" => {
            if let Some(d) = candidate.finished.get_mut(idx) {
                d.state = state;
            }
        }
        _ => unreachable!(),
    }
}

/// Roll back a `begin_deletion` transition after `execute_deletion`
/// fails to remove the file. Restores the persisted task state from
/// `deleting_previous_state` and clears the flag so the recording
/// reverts to its pre-deletion state. Best-effort: missing or already
/// finalized tasks are silently left alone.
pub fn rollback_deletion(candidate: &mut PersistedDownloadQueue, uuid: &str) {
    let Some((bucket, idx)) = locate(candidate, uuid) else { return };
    let task = match bucket {
        "queue" => candidate.queue.get_mut(idx),
        "scheduled" => candidate.scheduled.get_mut(idx),
        "active" => candidate.active.as_mut(),
        "finished" => candidate.finished.get_mut(idx),
        _ => None,
    };
    let Some(task) = task else { return };
    let Some(meta) = task.recording.as_mut() else { return };
    let prior = meta.deleting_previous_state.take();
    task.state = match prior {
        Some(DeletionPreviousState::Completed) => DownloadState::Completed,
        Some(DeletionPreviousState::Failed) => DownloadState::Failed,
        Some(DeletionPreviousState::Cancelled) => DownloadState::Cancelled,
        // No recorded prior state (the begin step never ran, or the
        // recording was already terminal) — fall back to the natural
        // non-terminal state. The scheduler will not reissue a delete
        // for a recording it never observed as Deleting.
        None => DownloadState::Scheduled,
    };
}

/// Resolve the file path to unlink for a recording. Only the
/// state-owned file is removed. Terminal `Completed` → final; `Failed`/
/// `Cancelled` → partial (a failed/cancelled recording never reached
/// finalization).
///
/// The returned path is **canonicalized** when the file exists on
/// disk. `recovery_action_for` compares the result against
/// `recording_root` to detect a path that escapes the recording
/// directory; a raw `download.file_path` may still carry literal `..`
/// segments (`dir/../outside.ts`) that defeat a lexical `starts_with`,
/// so canonicalization is the only honest way to decide whether the
/// file is actually inside the root.
///
/// `recording_root` is accepted for symmetry with the recovery
/// caller; today every site passes `None` and resolves the path from
/// the task itself, which is the right call while `RecordingMetadata`
/// still carries the absolute path verbatim.
pub async fn file_path_for_deletion(download: &FileDownload, _recording_root: Option<&Path>) -> Option<PathBuf> {
    let partial = crate::recording_worker::recording_partial_path(&download.file_path);
    let prior = download.recording.as_ref().and_then(|m| m.deleting_previous_state);
    let raw = match prior {
        Some(DeletionPreviousState::Completed) => download.file_path.clone(),
        Some(_) => partial,
        None => return None,
    };
    // Canonicalize to resolve `..` segments and symlinks. If the file
    // is missing or unreadable, `canonicalize` fails — keep the raw
    // path; the missing-file branch in the caller will turn it into
    // `FinishDeletion`/`NotDeleting` instead of running IO on it.
    Some(tokio::fs::canonicalize(&raw).await.unwrap_or(raw))
}

/// Unlink the file owned by a `DeletionTarget`. Missing files count as
/// success. Returns the path that was unlinked, or `None` if no
/// physical file was present.
pub async fn execute_deletion_target(target: &DeletionTarget) -> Result<Option<PathBuf>, DeletionError> {
    unlink_owned_file(&target.path_to_unlink()).await
}

async fn unlink_owned_file(path: &Path) -> Result<Option<PathBuf>, DeletionError> {
    if no_follow_existing(path).await.is_none() {
        return Ok(None);
    }
    safe_unlink(path).await.map_err(std::io::Error::from).map_err(DeletionError::DeleteFailed)?;
    Ok(Some(path.to_path_buf()))
}

/// Unlink the recorded file. Missing files count as
/// success. Returns the path that was unlinked, or `None` if no
/// physical file was present.
pub async fn execute_deletion(
    download: &FileDownload,
    recording_root: Option<&Path>,
) -> Result<Option<PathBuf>, DeletionError> {
    let Some(path) = file_path_for_deletion(download, recording_root).await else {
        return Ok(None);
    };
    unlink_owned_file(&path).await
}

/// Remove the task from the queue under a new mutation
/// boundary. Called after the file is gone (or was already missing).
pub async fn finalize_deletion(queue: &DownloadQueue, uuid: &str) -> Result<(), DeletionError> {
    crate::download::mutate(queue, |candidate| {
        if let Some((bucket, idx)) = locate(candidate, uuid) {
            match bucket {
                "queue" => {
                    if idx < candidate.queue.len() {
                        candidate.queue.remove(idx);
                    }
                }
                "scheduled" => {
                    if idx < candidate.scheduled.len() {
                        candidate.scheduled.remove(idx);
                    }
                }
                "active" => {
                    candidate.active = None;
                }
                "finished" => {
                    if idx < candidate.finished.len() {
                        candidate.finished.remove(idx);
                    }
                }
                _ => unreachable!(),
            }
            return Ok(());
        }
        Err(QueueMutationError::UnknownRecording)
    })
    .await
    .map_err(DeletionError::FinalizeFailed)?;
    Ok(())
}

/// Startup recovery decision for a task in `Deleting` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// The file is already gone; finish the deletion by removing the
    /// task.
    FinishDeletion,
    /// The file is still present as a regular file; restore the
    /// previous state and clear `deleting_previous_state`.
    RestorePrevious,
    /// The path is unsafe (symlink, wrong type); log and restore the
    /// previous state.
    UnsafeRestore,
    /// The recording has no `deleting_previous_state`; nothing to do.
    NotDeleting,
}

/// Inspect a task that is in `Deleting` state and decide what the
/// startup recovery should do.
pub async fn recovery_action_for(download: &FileDownload, recording_root: Option<&Path>) -> RecoveryAction {
    let Some(meta) = &download.recording else { return RecoveryAction::NotDeleting };
    let Some(_prior) = meta.deleting_previous_state else { return RecoveryAction::NotDeleting };
    let Some(path) = file_path_for_deletion(download, recording_root).await else {
        return RecoveryAction::FinishDeletion;
    };
    if no_follow_existing(&path).await.is_none() {
        return RecoveryAction::FinishDeletion;
    }
    // The file is still present. If the metadata-derived path is outside
    // the recording root, treat as unsafe; otherwise restore the
    // previous state. `path` already comes back canonicalized from
    // `file_path_for_deletion` (so literal `..` segments cannot slip
    // past the lexical check); only `root` needs canonicalizing here.
    if let Some(root) = recording_root {
        let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if !path.starts_with(&root_canon) {
            return RecoveryAction::UnsafeRestore;
        }
    }
    RecoveryAction::RestorePrevious
}

/// Apply the recovery action to a candidate. Called by the startup
/// loop after the decision has been computed.
pub fn apply_recovery_to_candidate(candidate: &mut PersistedDownloadQueue, uuid: &str, action: RecoveryAction) {
    if let Some((bucket, idx)) = locate(candidate, uuid) {
        let d = match bucket {
            "queue" => candidate.queue.get_mut(idx),
            "scheduled" => candidate.scheduled.get_mut(idx),
            "active" => candidate.active.as_mut(),
            "finished" => candidate.finished.get_mut(idx),
            _ => None,
        };
        if let Some(d) = d {
            match action {
                RecoveryAction::FinishDeletion => {
                    // The caller is expected to remove the task entirely
                    // after this marker is applied. Here we just clear
                    // the deletion marker so the post-removal state is
                    // consistent if a higher-level caller decides
                    // otherwise.
                    if let Some(meta) = d.recording.as_mut() {
                        meta.deleting_previous_state = None;
                    }
                }
                RecoveryAction::RestorePrevious | RecoveryAction::UnsafeRestore => {
                    restore_previous_state(d);
                }
                RecoveryAction::NotDeleting => {}
            }
        }
    }
}

fn restore_previous_state(d: &mut PersistedFileDownload) {
    let prior = d.recording.as_ref().and_then(|m| m.deleting_previous_state);
    let Some(prior) = prior else { return };
    d.state = match prior {
        DeletionPreviousState::Completed => DownloadState::Completed,
        DeletionPreviousState::Failed => DownloadState::Failed,
        DeletionPreviousState::Cancelled => DownloadState::Cancelled,
    };
    if let Some(meta) = d.recording.as_mut() {
        meta.deleting_previous_state = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::{mutate, DownloadKind, DownloadState, PersistedDownloadQueue, PersistedFileDownload};
    use shared::model::RecordingMetadata;
    use std::{path::PathBuf, sync::atomic::Ordering};
    use tempfile::TempDir;

    fn make_persisted_recording(
        uuid: &str,
        state: DownloadState,
        deleting: Option<DeletionPreviousState>,
    ) -> PersistedFileDownload {
        let mut meta = RecordingMetadata::for_legacy_admin(1_700_000_000, 60);
        meta.deleting_previous_state = deleting;
        PersistedFileDownload {
            uuid: uuid.to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from(format!("/tmp/{uuid}.ts")),
            filename: format!("{uuid}.ts"),
            url: format!("https://example.com/{uuid}"),
            finished: matches!(state, DownloadState::Completed),
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state,
            start_at: Some(0),
            duration_secs: Some(60),
            kind: DownloadKind::Recording,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: Some(meta),
        }
    }

    fn finished_with_state(uuid: &str, state: DownloadState, deleting: Option<DeletionPreviousState>) -> FileDownload {
        let p = make_persisted_recording(uuid, state, deleting);
        crate::download::DownloadQueue::from_persisted_with(p, None, None).expect("restore")
    }

    #[test]
    fn prior_terminal_state_accepts_completed_failed_cancelled_only() {
        let p = make_persisted_recording("r", DownloadState::Completed, None);
        let task =
            crate::download::DownloadQueue::from_persisted_with(p, None, None).expect("test fixture must be valid");
        assert_eq!(prior_terminal_state(&task), Some(DeletionPreviousState::Completed));
        let p = make_persisted_recording("r", DownloadState::Failed, None);
        let task =
            crate::download::DownloadQueue::from_persisted_with(p, None, None).expect("test fixture must be valid");
        assert_eq!(prior_terminal_state(&task), Some(DeletionPreviousState::Failed));
        let p = make_persisted_recording("r", DownloadState::Cancelled, None);
        let task =
            crate::download::DownloadQueue::from_persisted_with(p, None, None).expect("test fixture must be valid");
        assert_eq!(prior_terminal_state(&task), Some(DeletionPreviousState::Cancelled));
        let p = make_persisted_recording("r", DownloadState::Downloading, None);
        let task =
            crate::download::DownloadQueue::from_persisted_with(p, None, None).expect("test fixture must be valid");
        assert_eq!(prior_terminal_state(&task), None);
    }

    #[tokio::test]
    async fn file_path_for_deletion_uses_final_for_completed_partial_otherwise() {
        let task = finished_with_state("r", DownloadState::Completed, Some(DeletionPreviousState::Completed));
        let path = file_path_for_deletion(&task, None).await.expect("path");
        assert_eq!(path, PathBuf::from("/tmp/r.ts"));
        let task = finished_with_state("r", DownloadState::Failed, Some(DeletionPreviousState::Failed));
        let path = file_path_for_deletion(&task, None).await.expect("path");
        assert_eq!(path, PathBuf::from("/tmp/r.ts.partial"));
    }

    #[tokio::test]
    async fn execute_deletion_unlinks_existing_file_and_returns_path() {
        let dir = TempDir::new().expect("tempdir");
        // `execute_deletion` returns the canonicalized path. On macOS the temp
        // directory sits behind the `/var` -> `/private/var` symlink, so the
        // fixture path must be canonicalized too; otherwise the assertion
        // compares two spellings of the same file and always fails there.
        let dir_path = dir.path().canonicalize().expect("canonical tempdir");
        let final_path = dir_path.join("r.ts");
        tokio::fs::write(&final_path, b"data").await.expect("write");
        let mut task = finished_with_state("r", DownloadState::Completed, Some(DeletionPreviousState::Completed));
        task.file_path = final_path.clone();
        let deleted = execute_deletion(&task, None).await.expect("delete").expect("some path");
        assert_eq!(deleted, final_path);
        assert!(!final_path.exists());
    }

    #[tokio::test]
    async fn execute_deletion_is_idempotent_for_missing_file() {
        let dir = TempDir::new().expect("tempdir");
        let mut task = finished_with_state("r", DownloadState::Completed, None);
        task.file_path = dir.path().join("does-not-exist.ts");
        let result = execute_deletion(&task, None).await.expect("ok");
        assert!(result.is_none(), "missing file must report no path");
    }

    #[tokio::test]
    async fn recovery_action_for_finish_when_file_missing() {
        let dir = TempDir::new().expect("tempdir");
        let mut task = finished_with_state("r", DownloadState::Completed, Some(DeletionPreviousState::Completed));
        task.file_path = dir.path().join("missing.ts");
        assert_eq!(recovery_action_for(&task, Some(dir.path())).await, RecoveryAction::FinishDeletion);
    }

    #[tokio::test]
    async fn recovery_action_for_restore_when_regular_file_present() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("r.ts");
        tokio::fs::write(&final_path, b"data").await.expect("write");
        let mut task = finished_with_state("r", DownloadState::Completed, Some(DeletionPreviousState::Completed));
        task.file_path = final_path;
        assert_eq!(recovery_action_for(&task, Some(dir.path())).await, RecoveryAction::RestorePrevious);
    }

    #[tokio::test]
    async fn recovery_action_for_unsafe_when_path_outside_root() {
        let dir = TempDir::new().expect("tempdir");
        let outside = dir.path().join("..").join("outside.ts");
        let outside = outside.canonicalize().unwrap_or(outside);
        tokio::fs::write(&outside, b"data").await.expect("write");
        let mut task = finished_with_state("r", DownloadState::Completed, Some(DeletionPreviousState::Completed));
        task.file_path = outside;
        assert_eq!(recovery_action_for(&task, Some(dir.path())).await, RecoveryAction::UnsafeRestore);
    }

    #[tokio::test]
    async fn recovery_action_for_not_deleting_when_marker_absent() {
        let dir = TempDir::new().expect("tempdir");
        let mut task = finished_with_state("r", DownloadState::Completed, None);
        task.file_path = dir.path().join("r.ts");
        assert_eq!(recovery_action_for(&task, Some(dir.path())).await, RecoveryAction::NotDeleting);
    }

    #[test]
    fn apply_recovery_to_candidate_restores_state() {
        let mut candidate = PersistedDownloadQueue::default();
        let mut p = make_persisted_recording("r", DownloadState::Cancelled, Some(DeletionPreviousState::Completed));
        p.state = DownloadState::Cancelled;
        candidate.finished.push(p);
        apply_recovery_to_candidate(&mut candidate, "r", RecoveryAction::RestorePrevious);
        let restored = &candidate.finished[0];
        assert_eq!(restored.state, DownloadState::Completed);
        assert!(restored.recording.as_ref().unwrap().deleting_previous_state.is_none());
    }

    #[tokio::test]
    async fn begin_deletion_stamps_deleting_state_under_boundary() {
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
        let mut task = finished_with_state("r", DownloadState::Completed, None);
        task.file_path = dir.path().join("r.ts");
        let persisted = DownloadQueue::to_persisted(&task);
        mutate(&queue, |c| {
            c.finished.push(persisted);
            Ok(())
        })
        .await
        .expect("seed");
        let prior = queue.revision.load(Ordering::SeqCst);
        begin_deletion(&queue, "r").await.expect("begin");
        let after = queue.finished.read().await.first().cloned().expect("task");
        assert_eq!(after.recording.as_ref().unwrap().deleting_previous_state, Some(DeletionPreviousState::Completed));
        assert!(after.recording.as_ref().unwrap().is_deleting());
        assert!(queue.revision.load(Ordering::SeqCst) > prior, "revision must advance");
    }

    #[tokio::test]
    async fn begin_deletion_authorized_rejects_when_the_permit_declines() {
        // Authorization runs inside the same mutation boundary that stamps
        // the task, so a decline must leave the task untouched.
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file));
        let mut task = finished_with_state("r", DownloadState::Completed, None);
        task.file_path = dir.path().join("r.ts");
        let persisted = DownloadQueue::to_persisted(&task);
        mutate(&queue, |c| {
            c.finished.push(persisted);
            Ok(())
        })
        .await
        .expect("seed");

        let result = begin_deletion_authorized(&queue, "r", |_| false).await;

        assert!(matches!(result, Err(DeletionError::Forbidden)));
        let finished = queue.finished.read().await;
        assert_eq!(finished[0].state, DownloadState::Completed);
        assert!(finished[0].recording.as_ref().is_none_or(|meta| meta.deleting_previous_state.is_none()));
    }

    #[tokio::test]
    async fn begin_deletion_rejects_unknown_task() {
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file));
        let result = begin_deletion(&queue, "missing").await;
        // Reported as its own variant now, not folded into the opaque
        // `BeginFailed`, so the service layer can map it to a 404.
        assert!(matches!(result, Err(DeletionError::UnknownTask)));
    }

    #[tokio::test]
    async fn begin_deletion_rejects_non_terminal_state() {
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file));
        let mut task = finished_with_state("r", DownloadState::Downloading, None);
        task.file_path = dir.path().join("r.ts");
        let persisted = DownloadQueue::to_persisted(&task);
        mutate(&queue, |c| {
            c.finished.push(persisted);
            Ok(())
        })
        .await
        .expect("seed");
        let result = begin_deletion(&queue, "r").await;
        assert!(matches!(result, Err(DeletionError::NotTerminal)));
    }

    #[tokio::test]
    async fn finalize_deletion_removes_task_under_boundary() {
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file));
        let mut task = finished_with_state("r", DownloadState::Cancelled, Some(DeletionPreviousState::Cancelled));
        task.file_path = dir.path().join("r.ts");
        let persisted = DownloadQueue::to_persisted(&task);
        mutate(&queue, |c| {
            c.finished.push(persisted);
            Ok(())
        })
        .await
        .expect("seed");
        finalize_deletion(&queue, "r").await.expect("finalize");
        assert!(queue.finished.read().await.is_empty());
    }
}
