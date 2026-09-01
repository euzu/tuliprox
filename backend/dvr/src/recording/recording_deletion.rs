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

use crate::recording::recording_queue::{
    PersistedRecordingQueue, PersistedRecordingTask, QueueMutationError, RecordingQueue, RecordingTask,
    RecordingTaskState,
};
use shared::model::{DeletionPreviousState, RecordingMetadata};
use std::path::{Path, PathBuf};
use tuliprox_core::utils::{no_follow_existing, safe_unlink};

/// Errors that can occur during the three phases.
#[derive(Debug)]
pub enum DeletionError {
    /// The UUID did not match any task in the queue.
    UnknownTask,
    /// The matched task is not a recording.
    NotARecording,
    /// The matched task is not in a terminal state, so deletion cannot
    /// begin.
    NotTerminal,
    /// The caller is not permitted to delete this recording. Reported
    /// from inside the mutation boundary so authorization and the state
    /// transition observe the same task.
    Forbidden,
    /// Marking the recording as `Deleting` failed.
    BeginFailed(QueueMutationError),
    /// File deletion failed in a way that is not safe to
    /// ignore.
    DeleteFailed(std::io::Error),
    /// Removing the task from the queue failed.
    FinalizeFailed(QueueMutationError),
}

impl std::fmt::Display for DeletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTask => f.write_str("recording not found"),
            Self::NotARecording => f.write_str("task is not a recording"),
            Self::NotTerminal => f.write_str("recording is not in a terminal state"),
            Self::Forbidden => f.write_str("recording deletion forbidden"),
            Self::BeginFailed(err) => write!(f, "begin deletion failed: {err}"),
            Self::DeleteFailed(err) => write!(f, "physical delete failed: {err}"),
            Self::FinalizeFailed(err) => write!(f, "finalize deletion failed: {err}"),
        }
    }
}

impl std::error::Error for DeletionError {}

/// Locate a recording task in the candidate by uuid. Returns
/// `(bucket, index)` where `bucket` is one of `"queue"`, `"scheduled"`,
/// `"active"`, `"finished"`. Returns `None` if the uuid is not in the
/// candidate.
fn locate(candidate: &PersistedRecordingQueue, uuid: &str) -> Option<(&'static str, usize)> {
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
fn read_meta(candidate: &PersistedRecordingQueue, uuid: &str) -> Option<RecordingMetadata> {
    candidate
        .queue
        .iter()
        .chain(candidate.scheduled.iter())
        .chain(candidate.active.iter())
        .chain(candidate.finished.iter())
        .find(|task| task.uuid == uuid)
        .map(|task| task.recording.clone())
}

/// Derive the prior terminal state from a recording's current state.
/// Returns `None` when the state is not a terminal one (and therefore
/// deletion cannot begin).
#[cfg(test)]
fn prior_terminal_state(download: &RecordingTask) -> Option<DeletionPreviousState> {
    match download.state {
        RecordingTaskState::Completed => Some(DeletionPreviousState::Completed),
        RecordingTaskState::Failed => Some(DeletionPreviousState::Failed),
        RecordingTaskState::Cancelled => Some(DeletionPreviousState::Cancelled),
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
    /// Another library entry still points at this file, so removing this
    /// entry must leave the bytes alone.
    pub still_referenced: bool,
}

impl DeletionTarget {
    /// The file this deletion may unlink, or `None` when the entry is one
    /// of several holding it. `Completed` recordings own their final file;
    /// `Failed` / `Cancelled` ones never reached finalization, so they own
    /// the `.partial`.
    pub fn path_to_unlink(&self) -> Option<PathBuf> {
        if self.still_referenced {
            return None;
        }
        Some(match self.previous_state {
            DeletionPreviousState::Completed => self.file_path.clone(),
            DeletionPreviousState::Failed | DeletionPreviousState::Cancelled => {
                crate::recording_worker::recording_partial_path(&self.file_path)
            }
        })
    }
}

/// Mark the recording as `Deleting` under the queue mutation boundary,
/// with `permit` deciding — inside that same boundary — whether the
/// caller may do so. The candidate is persisted atomically with the new
/// `deleting_previous_state`; on success the in-memory queue reflects
/// `Deleting` and the on-disk file is unchanged.
pub async fn begin_deletion_authorized<F>(
    queue: &RecordingQueue,
    uuid: &str,
    permit: F,
) -> Result<DeletionTarget, DeletionError>
where
    F: FnOnce(&RecordingMetadata) -> bool,
{
    crate::recording::recording_queue::mutate(queue, |candidate| {
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
        let target = DeletionTarget {
            uuid: uuid.to_string(),
            file_path: task.file_path.clone(),
            previous_state: prior,
            still_referenced: crate::recording::recording_queue::media_is_still_referenced(candidate, uuid),
        };
        let mut new_meta = meta;
        new_meta.deleting_previous_state = Some(prior);
        apply_meta(candidate, bucket, idx, new_meta);
        set_task_state(candidate, bucket, idx, RecordingTaskState::Cancelled);
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
pub async fn begin_deletion(queue: &RecordingQueue, uuid: &str) -> Result<DeletionTarget, DeletionError> {
    begin_deletion_authorized(queue, uuid, |_| true).await
}

fn prior_terminal_state_runtime(download: &PersistedRecordingTask) -> Option<DeletionPreviousState> {
    // A stamped task's live `state` is the deleting marker, not its terminal
    // state; deriving from it would call a Completed recording Cancelled and
    // unlink the partial instead of the file.
    if download.recording.deleting_previous_state.is_some() {
        return None;
    }
    match download.state {
        RecordingTaskState::Completed => Some(DeletionPreviousState::Completed),
        RecordingTaskState::Failed => Some(DeletionPreviousState::Failed),
        RecordingTaskState::Cancelled => Some(DeletionPreviousState::Cancelled),
        _ => None,
    }
}

fn apply_meta(candidate: &mut PersistedRecordingQueue, bucket: &'static str, idx: usize, meta: RecordingMetadata) {
    match bucket {
        "queue" => {
            if let Some(d) = candidate.queue.get_mut(idx) {
                d.recording = meta;
            }
        }
        "scheduled" => {
            if let Some(d) = candidate.scheduled.get_mut(idx) {
                d.recording = meta;
            }
        }
        "active" => {
            if let Some(d) = candidate.active.as_mut() {
                d.recording = meta;
            }
        }
        "finished" => {
            if let Some(d) = candidate.finished.get_mut(idx) {
                d.recording = meta;
            }
        }
        _ => unreachable!(),
    }
}

/// Set the persisted `state` on a task in the given bucket. Companion to
/// `apply_meta` — the deletion transition needs both the recording
/// metadata flag (`deleting_previous_state`) and the canonical task
/// state (`Cancelled`) to land atomically.
fn set_task_state(
    candidate: &mut PersistedRecordingQueue,
    bucket: &'static str,
    idx: usize,
    state: RecordingTaskState,
) {
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
pub fn rollback_deletion(candidate: &mut PersistedRecordingQueue, uuid: &str) {
    let Some((bucket, idx)) = locate(candidate, uuid) else { return };
    let task = match bucket {
        "queue" => candidate.queue.get_mut(idx),
        "scheduled" => candidate.scheduled.get_mut(idx),
        "active" => candidate.active.as_mut(),
        "finished" => candidate.finished.get_mut(idx),
        _ => None,
    };
    let Some(task) = task else { return };
    let prior = task.recording.deleting_previous_state.take();
    task.state = match prior {
        Some(DeletionPreviousState::Completed) => RecordingTaskState::Completed,
        Some(DeletionPreviousState::Failed) => RecordingTaskState::Failed,
        Some(DeletionPreviousState::Cancelled) => RecordingTaskState::Cancelled,
        // No recorded prior state (the begin step never ran, or the
        // recording was already terminal) — fall back to the natural
        // non-terminal state. The scheduler will not reissue a delete
        // for a recording it never observed as Deleting.
        None => RecordingTaskState::Scheduled,
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
pub async fn file_path_for_deletion(download: &RecordingTask, _recording_root: Option<&Path>) -> Option<PathBuf> {
    let partial = crate::recording_worker::recording_partial_path(&download.file_path);
    let prior = download.recording.deleting_previous_state;
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
    let Some(path) = target.path_to_unlink() else {
        return Ok(None);
    };
    unlink_owned_file(&path).await
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
    download: &RecordingTask,
    recording_root: Option<&Path>,
) -> Result<Option<PathBuf>, DeletionError> {
    let Some(path) = file_path_for_deletion(download, recording_root).await else {
        return Ok(None);
    };
    unlink_owned_file(&path).await
}

/// Remove the task from the queue under a new mutation
/// boundary. Called after the file is gone (or was already missing).
pub async fn finalize_deletion(queue: &RecordingQueue, uuid: &str) -> Result<(), DeletionError> {
    crate::recording::recording_queue::mutate(queue, |candidate| {
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
///
/// `still_referenced` says another entry holds this file. A present file then
/// proves nothing about whether the deletion ran, because a correct deletion
/// deliberately leaves a shared file alone.
pub async fn recovery_action_for(
    download: &RecordingTask,
    recording_root: Option<&Path>,
    still_referenced: bool,
) -> RecoveryAction {
    let Some(_prior) = download.recording.deleting_previous_state else { return RecoveryAction::NotDeleting };
    if still_referenced {
        return RecoveryAction::FinishDeletion;
    }
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
pub fn apply_recovery_to_candidate(candidate: &mut PersistedRecordingQueue, uuid: &str, action: RecoveryAction) {
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
                    d.recording.deleting_previous_state = None;
                }
                RecoveryAction::RestorePrevious | RecoveryAction::UnsafeRestore => {
                    restore_previous_state(d);
                }
                RecoveryAction::NotDeleting => {}
            }
        }
    }
}

fn restore_previous_state(d: &mut PersistedRecordingTask) {
    let Some(prior) = d.recording.deleting_previous_state else { return };
    d.state = match prior {
        DeletionPreviousState::Completed => RecordingTaskState::Completed,
        DeletionPreviousState::Failed => RecordingTaskState::Failed,
        DeletionPreviousState::Cancelled => RecordingTaskState::Cancelled,
    };
    d.recording.deleting_previous_state = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::recording_queue::{
        mutate, PersistedRecordingQueue, PersistedRecordingTask, RecordingTaskState,
    };
    use shared::model::{
        RecordingKind, RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility, UserId,
    };
    use std::{path::PathBuf, sync::atomic::Ordering};
    use tempfile::TempDir;

    fn make_persisted_recording(
        uuid: &str,
        state: RecordingTaskState,
        deleting: Option<DeletionPreviousState>,
    ) -> PersistedRecordingTask {
        let mut meta = RecordingMetadata::new_live(
            RecordingOwner::User(UserId::from("web:alice")),
            RecordingVisibility::Private,
            RecordingSource::new("t1", "v1", "in1"),
            1_700_000_000,
            1_700_000_060,
            0,
            0,
        );
        meta.deleting_previous_state = deleting;
        PersistedRecordingTask {
            media_identity: String::new(),
            partition: crate::recording::recording_queue::RecordingPartition::default(),
            uuid: uuid.to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from(format!("/tmp/{uuid}.ts")),
            filename: format!("{uuid}.ts"),
            url: format!("https://example.com/{uuid}"),
            finished: matches!(state, RecordingTaskState::Completed),
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state,
            kind: RecordingKind::Live,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: meta,
        }
    }

    fn finished_with_state(
        uuid: &str,
        state: RecordingTaskState,
        deleting: Option<DeletionPreviousState>,
    ) -> RecordingTask {
        let p = make_persisted_recording(uuid, state, deleting);
        crate::recording::recording_queue::RecordingQueue::from_persisted(p).expect("restore")
    }

    #[test]
    fn prior_terminal_state_accepts_completed_failed_cancelled_only() {
        let p = make_persisted_recording("r", RecordingTaskState::Completed, None);
        let task =
            crate::recording::recording_queue::RecordingQueue::from_persisted(p).expect("test fixture must be valid");
        assert_eq!(prior_terminal_state(&task), Some(DeletionPreviousState::Completed));
        let p = make_persisted_recording("r", RecordingTaskState::Failed, None);
        let task =
            crate::recording::recording_queue::RecordingQueue::from_persisted(p).expect("test fixture must be valid");
        assert_eq!(prior_terminal_state(&task), Some(DeletionPreviousState::Failed));
        let p = make_persisted_recording("r", RecordingTaskState::Cancelled, None);
        let task =
            crate::recording::recording_queue::RecordingQueue::from_persisted(p).expect("test fixture must be valid");
        assert_eq!(prior_terminal_state(&task), Some(DeletionPreviousState::Cancelled));
        let p = make_persisted_recording("r", RecordingTaskState::Running, None);
        let task =
            crate::recording::recording_queue::RecordingQueue::from_persisted(p).expect("test fixture must be valid");
        assert_eq!(prior_terminal_state(&task), None);
    }

    #[tokio::test]
    async fn file_path_for_deletion_uses_final_for_completed_partial_otherwise() {
        let task = finished_with_state("r", RecordingTaskState::Completed, Some(DeletionPreviousState::Completed));
        let path = file_path_for_deletion(&task, None).await.expect("path");
        assert_eq!(path, PathBuf::from("/tmp/r.ts"));
        let task = finished_with_state("r", RecordingTaskState::Failed, Some(DeletionPreviousState::Failed));
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
        let mut task = finished_with_state("r", RecordingTaskState::Completed, Some(DeletionPreviousState::Completed));
        task.file_path = final_path.clone();
        let deleted = execute_deletion(&task, None).await.expect("delete").expect("some path");
        assert_eq!(deleted, final_path);
        assert!(!final_path.exists());
    }

    #[tokio::test]
    async fn execute_deletion_is_idempotent_for_missing_file() {
        let dir = TempDir::new().expect("tempdir");
        let mut task = finished_with_state("r", RecordingTaskState::Completed, None);
        task.file_path = dir.path().join("does-not-exist.ts");
        let result = execute_deletion(&task, None).await.expect("ok");
        assert!(result.is_none(), "missing file must report no path");
    }

    #[tokio::test]
    async fn recovery_action_for_finish_when_file_missing() {
        let dir = TempDir::new().expect("tempdir");
        let mut task = finished_with_state("r", RecordingTaskState::Completed, Some(DeletionPreviousState::Completed));
        task.file_path = dir.path().join("missing.ts");
        assert_eq!(recovery_action_for(&task, Some(dir.path()), false).await, RecoveryAction::FinishDeletion);
    }

    #[tokio::test]
    async fn recovery_action_for_restore_when_regular_file_present() {
        let dir = TempDir::new().expect("tempdir");
        let final_path = dir.path().join("r.ts");
        tokio::fs::write(&final_path, b"data").await.expect("write");
        let mut task = finished_with_state("r", RecordingTaskState::Completed, Some(DeletionPreviousState::Completed));
        task.file_path = final_path;
        assert_eq!(recovery_action_for(&task, Some(dir.path()), false).await, RecoveryAction::RestorePrevious);
    }

    /// Every combination of the four inputs the recovery decision reads.
    ///
    /// The rules, in the order the decision applies them:
    ///   no deleting marker                 -> `NotDeleting`
    ///   another entry holds the file       -> `FinishDeletion`
    ///   the file is gone                   -> `FinishDeletion`
    ///   the file is here, inside the root  -> `RestorePrevious`
    ///   the file is here, outside the root -> `UnsafeRestore`
    #[tokio::test]
    async fn the_recovery_decision_table_is_exhaustive() {
        for marker in [None, Some(DeletionPreviousState::Completed)] {
            for still_referenced in [false, true] {
                for file_present in [false, true] {
                    for inside_root in [false, true] {
                        let dir = TempDir::new().expect("tempdir");
                        let elsewhere = TempDir::new().expect("tempdir");
                        let mut task = finished_with_state("r", RecordingTaskState::Cancelled, marker);
                        task.file_path = dir.path().join("r.ts");
                        if file_present {
                            std::fs::write(&task.file_path, b"bytes").expect("write");
                        }
                        let root = if inside_root { dir.path() } else { elsewhere.path() };

                        let expected = if marker.is_none() {
                            RecoveryAction::NotDeleting
                        } else if still_referenced || !file_present {
                            RecoveryAction::FinishDeletion
                        } else if inside_root {
                            RecoveryAction::RestorePrevious
                        } else {
                            RecoveryAction::UnsafeRestore
                        };

                        assert_eq!(
                            recovery_action_for(&task, Some(root), still_referenced).await,
                            expected,
                            "marker={marker:?} still_referenced={still_referenced} \
                             file_present={file_present} inside_root={inside_root}"
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn a_shared_file_outside_the_root_finishes_rather_than_flagging_the_path() {
        // Deliberate: the unsafe-path warning exists to stop an unlink nobody
        // can vouch for. A shared file is never unlinked here, so there is
        // nothing to stop -- and restoring would resurrect a deleted entry.
        // The diagnostic is traded for not undoing the user's deletion.
        let dir = TempDir::new().expect("tempdir");
        let elsewhere = TempDir::new().expect("tempdir");
        let mut task = finished_with_state("r", RecordingTaskState::Cancelled, Some(DeletionPreviousState::Completed));
        task.file_path = dir.path().join("r.ts");
        std::fs::write(&task.file_path, b"bytes").expect("write");
        assert_eq!(recovery_action_for(&task, Some(elsewhere.path()), true).await, RecoveryAction::FinishDeletion);
    }

    #[tokio::test]
    async fn an_interrupted_deletion_of_a_shared_file_still_finishes() {
        // A correct deletion leaves a shared file alone, so a present file no
        // longer proves the unlink never ran. Restoring here would silently
        // undo the user's deletion on the next boot.
        let dir = TempDir::new().expect("tempdir");
        let mut task = finished_with_state("r", RecordingTaskState::Cancelled, Some(DeletionPreviousState::Completed));
        task.file_path = dir.path().join("r.ts");
        std::fs::write(&task.file_path, b"held by another entry").expect("write");
        assert_eq!(recovery_action_for(&task, Some(dir.path()), true).await, RecoveryAction::FinishDeletion);
        // Nobody else holds it: the file's presence does mean the unlink failed.
        assert_eq!(recovery_action_for(&task, Some(dir.path()), false).await, RecoveryAction::RestorePrevious);
    }

    #[tokio::test]
    async fn recovery_action_for_unsafe_when_path_outside_root() {
        let dir = TempDir::new().expect("tempdir");
        let outside = dir.path().join("..").join("outside.ts");
        let outside = outside.canonicalize().unwrap_or(outside);
        tokio::fs::write(&outside, b"data").await.expect("write");
        let mut task = finished_with_state("r", RecordingTaskState::Completed, Some(DeletionPreviousState::Completed));
        task.file_path = outside;
        assert_eq!(recovery_action_for(&task, Some(dir.path()), false).await, RecoveryAction::UnsafeRestore);
    }

    #[tokio::test]
    async fn recovery_action_for_not_deleting_when_marker_absent() {
        let dir = TempDir::new().expect("tempdir");
        let mut task = finished_with_state("r", RecordingTaskState::Completed, None);
        task.file_path = dir.path().join("r.ts");
        assert_eq!(recovery_action_for(&task, Some(dir.path()), false).await, RecoveryAction::NotDeleting);
    }

    #[test]
    fn apply_recovery_to_candidate_restores_state() {
        let mut candidate = PersistedRecordingQueue::default();
        let mut p =
            make_persisted_recording("r", RecordingTaskState::Cancelled, Some(DeletionPreviousState::Completed));
        p.state = RecordingTaskState::Cancelled;
        candidate.finished.push(p);
        apply_recovery_to_candidate(&mut candidate, "r", RecoveryAction::RestorePrevious);
        let restored = &candidate.finished[0];
        assert_eq!(restored.state, RecordingTaskState::Completed);
        assert!(restored.recording.deleting_previous_state.is_none());
    }

    #[tokio::test]
    async fn begin_deletion_stamps_deleting_state_under_boundary() {
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().to_path_buf();
        let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
        let mut task = finished_with_state("r", RecordingTaskState::Completed, None);
        task.file_path = dir.path().join("r.ts");
        let persisted = RecordingQueue::to_persisted(&task);
        mutate(&queue, |c| {
            c.finished.push(persisted);
            Ok(())
        })
        .await
        .expect("seed");
        let prior = queue.revision.load(Ordering::SeqCst);
        begin_deletion(&queue, "r").await.expect("begin");
        let after = queue.finished.read().await.first().cloned().expect("task");
        assert_eq!(after.recording.deleting_previous_state, Some(DeletionPreviousState::Completed));
        assert!(after.recording.is_deleting());
        assert!(queue.revision.load(Ordering::SeqCst) > prior, "revision must advance");
    }

    /// Seed two entries onto the same programme, so they resolve to one
    /// media identity and one file, and delete the first.
    async fn delete_one_of_two_entries_sharing(dir: &TempDir) -> (RecordingQueue, PathBuf, DeletionTarget) {
        let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
        let shared_file = dir.path().join("programme.ts");
        std::fs::write(&shared_file, b"recorded bytes").expect("write file");
        for (uuid, owner) in [("alice-entry", "web:alice"), ("bob-entry", "web:bob")] {
            let mut task = finished_with_state(uuid, RecordingTaskState::Completed, None);
            task.file_path.clone_from(&shared_file);
            task.recording.owner = RecordingOwner::User(UserId::from(owner));
            let persisted = RecordingQueue::to_persisted(&task);
            mutate(&queue, |c| {
                c.finished.push(persisted);
                Ok(())
            })
            .await
            .expect("seed");
        }
        let target = begin_deletion(&queue, "alice-entry").await.expect("begin");
        (queue, shared_file, target)
    }

    #[tokio::test]
    async fn deleting_one_of_two_entries_leaves_the_shared_file_alone() {
        // Alice and Bob hold the same recording. Alice removing hers must not
        // take Bob's copy with it.
        let dir = TempDir::new().expect("tempdir");
        let (queue, shared_file, target) = delete_one_of_two_entries_sharing(&dir).await;
        assert!(target.still_referenced, "Bob still holds this file");
        assert_eq!(target.path_to_unlink(), None);
        let unlinked = execute_deletion_target(&target).await.expect("execute");
        assert_eq!(unlinked, None, "nothing was unlinked");
        assert!(shared_file.exists(), "Bob's recording must survive Alice's deletion");
        finalize_deletion(&queue, "alice-entry").await.expect("finalize");
        let remaining = queue.finished.read().await.clone();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].uuid, "bob-entry");
    }

    #[tokio::test]
    async fn the_last_entry_to_be_deleted_removes_the_file() {
        let dir = TempDir::new().expect("tempdir");
        let (queue, shared_file, first) = delete_one_of_two_entries_sharing(&dir).await;
        execute_deletion_target(&first).await.expect("execute");
        finalize_deletion(&queue, "alice-entry").await.expect("finalize");

        let second = begin_deletion(&queue, "bob-entry").await.expect("begin");
        assert!(!second.still_referenced, "nobody else holds it now");
        let unlinked = execute_deletion_target(&second).await.expect("execute");
        assert!(unlinked.is_some());
        assert!(!shared_file.exists(), "the bytes go once the last entry does");
    }

    #[tokio::test]
    async fn a_concurrent_deletion_does_not_strand_the_file() {
        // Both entries are stamped before either unlinks. If each counted the
        // other as a holder, the file would outlive every entry pointing at it.
        let dir = TempDir::new().expect("tempdir");
        let (queue, shared_file, first) = delete_one_of_two_entries_sharing(&dir).await;
        let second = begin_deletion(&queue, "bob-entry").await.expect("begin");
        assert!(first.still_referenced);
        assert!(!second.still_referenced, "an entry already deleting is not a holder");
        execute_deletion_target(&first).await.expect("execute");
        execute_deletion_target(&second).await.expect("execute");
        assert!(!shared_file.exists(), "the file must not be left with nothing pointing at it");
    }

    #[tokio::test]
    async fn one_user_leaving_does_not_stop_or_delete_the_others_recording() {
        // A cancelled and removed their entry; B is mid-transfer on the same
        // media. A cancelled entry owns the `.partial`, so deleting A unlinks
        // exactly the file B is writing into -- the transfer would keep
        // streaming into an unlinked inode and B would end with nothing.
        let dir = TempDir::new().expect("tempdir");
        let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
        let final_path = dir.path().join("film.mp4");
        let partial = crate::recording_worker::recording_partial_path(&final_path);
        std::fs::write(&partial, b"bytes B is still writing").expect("write partial");

        let mut leaving = finished_with_state("alice-entry", RecordingTaskState::Cancelled, None);
        leaving.file_path.clone_from(&final_path);
        let mut recording = finished_with_state("bob-entry", RecordingTaskState::Running, None);
        recording.file_path.clone_from(&final_path);
        recording.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
        let (leaving, recording) = (RecordingQueue::to_persisted(&leaving), RecordingQueue::to_persisted(&recording));
        assert_eq!(leaving.media_identity, recording.media_identity, "fixture must share one media");
        mutate(&queue, move |candidate| {
            candidate.finished.push(leaving.clone());
            candidate.active = Some(recording.clone());
            Ok(())
        })
        .await
        .expect("seed");

        let target = begin_deletion(&queue, "alice-entry").await.expect("begin");
        assert!(target.still_referenced, "B is recording this media right now");
        execute_deletion_target(&target).await.expect("execute");
        finalize_deletion(&queue, "alice-entry").await.expect("finalize");

        assert!(partial.exists(), "B's in-flight transfer must keep its file");
        let still_recording = queue.active.read().await.clone().expect("B is still active");
        assert_eq!(still_recording.uuid, "bob-entry");
        assert_eq!(still_recording.state, RecordingTaskState::Running, "A leaving must not stop B");
    }

    #[tokio::test]
    async fn the_partial_is_removed_when_the_last_entry_cancels_out() {
        // The counterpart: with nobody else holding it, a cancelled entry does
        // own its partial and must not leak it.
        let dir = TempDir::new().expect("tempdir");
        let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
        let final_path = dir.path().join("film.mp4");
        let partial = crate::recording_worker::recording_partial_path(&final_path);
        std::fs::write(&partial, b"abandoned bytes").expect("write partial");

        let mut only = finished_with_state("only", RecordingTaskState::Cancelled, None);
        only.file_path.clone_from(&final_path);
        let only = RecordingQueue::to_persisted(&only);
        mutate(&queue, move |candidate| {
            candidate.finished.push(only.clone());
            Ok(())
        })
        .await
        .expect("seed");

        let target = begin_deletion(&queue, "only").await.expect("begin");
        assert!(!target.still_referenced);
        execute_deletion_target(&target).await.expect("execute");
        assert!(!partial.exists(), "an abandoned partial must not be left behind");
    }

    #[tokio::test]
    async fn a_second_deletion_of_the_same_entry_is_refused_not_misread() {
        // Retention and a user delete can reach the same recording. The stamp
        // overwrites `state` with the deleting marker, so a second pass reading
        // it would call a Completed recording Cancelled -- and a Cancelled
        // recording owns the `.partial`, so it would unlink the wrong path and
        // leave the real file behind while removing the entry that named it.
        let dir = TempDir::new().expect("tempdir");
        let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
        let final_path = dir.path().join("film.mp4");
        std::fs::write(&final_path, b"the recording").expect("write");
        let mut task = finished_with_state("r", RecordingTaskState::Completed, None);
        task.file_path.clone_from(&final_path);
        let persisted = RecordingQueue::to_persisted(&task);
        mutate(&queue, move |candidate| {
            candidate.finished.push(persisted.clone());
            Ok(())
        })
        .await
        .expect("seed");

        let first = begin_deletion(&queue, "r").await.expect("first deletion begins");
        assert_eq!(first.previous_state, DeletionPreviousState::Completed);
        assert_eq!(first.path_to_unlink(), Some(final_path.clone()), "the first owns the final file");

        let second = begin_deletion(&queue, "r").await;
        assert!(
            matches!(second, Err(DeletionError::NotTerminal)),
            "a deletion already in flight must be skipped, not restamped"
        );

        execute_deletion_target(&first).await.expect("execute");
        assert!(!final_path.exists(), "the recording the first deletion claimed is the one removed");
    }

    #[tokio::test]
    async fn even_a_principal_allowed_to_delete_cannot_unlink_a_referenced_file() {
        // Task 14: authorization decides whether an entry may be removed. It
        // does not decide whether the bytes go -- that is the reference rule,
        // and it takes no principal at all. An admin or the retention
        // supervisor removing their entry must still leave another user's
        // recording on disk.
        let dir = TempDir::new().expect("tempdir");
        let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
        let shared_file = dir.path().join("programme.ts");
        std::fs::write(&shared_file, b"recorded bytes").expect("write");
        for (uuid, owner) in [("admin-entry", "builtin:admin"), ("bob-entry", "web:bob")] {
            let mut task = finished_with_state(uuid, RecordingTaskState::Completed, None);
            task.file_path.clone_from(&shared_file);
            task.recording.owner = RecordingOwner::User(UserId::from(owner));
            let persisted = RecordingQueue::to_persisted(&task);
            mutate(&queue, move |candidate| {
                candidate.finished.push(persisted.clone());
                Ok(())
            })
            .await
            .expect("seed");
        }

        // The permit says yes, as it would for an administrator.
        let target = begin_deletion_authorized(&queue, "admin-entry", |_| true).await.expect("permitted");
        assert!(target.still_referenced);
        assert_eq!(target.path_to_unlink(), None, "permission does not override a live reference");
        execute_deletion_target(&target).await.expect("execute");
        assert!(shared_file.exists(), "Bob's recording survives an administrator removing their own entry");
    }

    #[tokio::test]
    async fn a_sole_entry_still_removes_its_file() {
        let dir = TempDir::new().expect("tempdir");
        let queue = RecordingQueue::new_persistent(dir.path(), dir.path()).expect("open recording repository");
        let file = dir.path().join("only.ts");
        std::fs::write(&file, b"bytes").expect("write");
        let mut task = finished_with_state("only", RecordingTaskState::Completed, None);
        task.file_path.clone_from(&file);
        let persisted = RecordingQueue::to_persisted(&task);
        mutate(&queue, |c| {
            c.finished.push(persisted);
            Ok(())
        })
        .await
        .expect("seed");
        let target = begin_deletion(&queue, "only").await.expect("begin");
        assert!(!target.still_referenced);
        execute_deletion_target(&target).await.expect("execute");
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn begin_deletion_authorized_rejects_when_the_permit_declines() {
        // Authorization runs inside the same mutation boundary that stamps
        // the task, so a decline must leave the task untouched.
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().to_path_buf();
        let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
        let mut task = finished_with_state("r", RecordingTaskState::Completed, None);
        task.file_path = dir.path().join("r.ts");
        let persisted = RecordingQueue::to_persisted(&task);
        mutate(&queue, |c| {
            c.finished.push(persisted);
            Ok(())
        })
        .await
        .expect("seed");

        let result = begin_deletion_authorized(&queue, "r", |_| false).await;

        assert!(matches!(result, Err(DeletionError::Forbidden)));
        let finished = queue.finished.read().await;
        assert_eq!(finished[0].state, RecordingTaskState::Completed);
        assert!(finished[0].recording.deleting_previous_state.is_none());
    }

    #[tokio::test]
    async fn begin_deletion_rejects_unknown_task() {
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().to_path_buf();
        let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
        let result = begin_deletion(&queue, "missing").await;
        // Reported as its own variant now, not folded into the opaque
        // `BeginFailed`, so the service layer can map it to a 404.
        assert!(matches!(result, Err(DeletionError::UnknownTask)));
    }

    #[tokio::test]
    async fn begin_deletion_rejects_non_terminal_state() {
        let dir = TempDir::new().expect("tempdir");
        let state_file = dir.path().to_path_buf();
        let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
        let mut task = finished_with_state("r", RecordingTaskState::Running, None);
        task.file_path = dir.path().join("r.ts");
        let persisted = RecordingQueue::to_persisted(&task);
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
        let state_file = dir.path().to_path_buf();
        let queue = RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository");
        let mut task = finished_with_state("r", RecordingTaskState::Cancelled, Some(DeletionPreviousState::Cancelled));
        task.file_path = dir.path().join("r.ts");
        let persisted = RecordingQueue::to_persisted(&task);
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
