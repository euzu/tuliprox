use super::*;
use crate::recording::recording_transition;
use chrono::Utc;
use shared::model::{
    recording::{RecordingOwner, RecordingSource, RecordingVisibility},
    QueueRevision, RecordingKind, RecordingMetadata, UserId,
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::{timeout, Duration};
use tuliprox_core::model::RecordingConfig;

fn temp_state_file(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_nanos();
    let dir = std::env::temp_dir().join(format!("tuliprox_{name}_{nanos}"));
    std::fs::create_dir_all(&dir).expect("create state dir");
    dir
}

fn live_meta(owner: &str, start: i64, duration: u64) -> RecordingMetadata {
    RecordingMetadata::new_live(
        RecordingOwner::User(UserId::from(owner)),
        RecordingVisibility::Private,
        RecordingSource::new("target", "v", "input-a"),
        start,
        start.saturating_add(duration.cast_signed()),
        0,
        0,
    )
}

fn media_meta(owner: &str) -> RecordingMetadata {
    RecordingMetadata::new_media(
        RecordingOwner::User(UserId::from(owner)),
        RecordingVisibility::Private,
        RecordingSource::new("target", "v", "input-a"),
        String::new(),
    )
}

fn task(uuid: &str, kind: RecordingKind, state: RecordingTaskState) -> RecordingTask {
    RecordingTask {
        uuid: uuid.to_string(),
        kind,
        file_dir: PathBuf::from("/tmp"),
        file_path: PathBuf::from(format!("/tmp/{uuid}.ts")),
        filename: format!("{uuid}.ts"),
        url: reqwest::Url::parse(&format!("https://example.com/{uuid}")).expect("valid url"),
        finished: false,
        size: 0,
        total_size: None,
        paused: false,
        error: None,
        state,
        input_name: None,
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: match kind {
            RecordingKind::Live => live_meta("web:alice", 1_700_000_000, 3_600),
            _ => media_meta("web:alice"),
        },
    }
}

fn identified(uuid: &str, identity: &str, state: RecordingTaskState) -> PersistedRecordingTask {
    let mut persisted = RecordingQueue::to_persisted(&task(uuid, RecordingKind::Vod, state));
    persisted.media_identity = identity.to_owned();
    persisted
}

fn organized_task(kind: RecordingKind, filename: &str, recording: RecordingMetadata) -> RecordingTask {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = RecordingConfig::from(&shared::model::RecordingConfigDto {
        directory: Some(dir.path().to_string_lossy().into_owned()),
        organize_into_directories: true,
        ..Default::default()
    });
    RecordingTask::new(kind, "https://example.com/item", filename, &cfg, None, 0, recording).expect("task")
}

/// Age the first recovery generation so the next commit wants a
/// checkpoint, and block the directory that checkpoint would be written
/// to: the checkpoint fails after the commit itself succeeded.
fn force_a_failing_checkpoint(dir: &Path) {
    let recovery = dir.join("recordings_library_recovery");
    let manifest = recovery.join("gen-00000000000000000001/manifest.bin");
    let bytes = std::fs::read(&manifest).expect("manifest");
    let mut payload: serde_json::Value = serde_json::from_slice(&bytes[40..]).expect("manifest payload");
    payload["created_at_unix"] = serde_json::json!(0);
    let payload = serde_json::to_vec(&payload).expect("encode");
    let mut framed = b"TRJ1".to_vec();
    framed.extend_from_slice(&u32::try_from(payload.len()).expect("length").to_le_bytes());
    framed.extend_from_slice(blake3::hash(&payload).as_bytes());
    framed.extend_from_slice(&payload);
    std::fs::write(&manifest, framed).expect("rewrite manifest");
    std::fs::write(recovery.join("gen-00000000000000000002"), b"blocks the next generation").expect("block");
}

// --- Transactional queue mutation boundary ---

fn make_test_recording_task(uuid: &str, file_path: PathBuf) -> RecordingTask {
    make_test_task_of_kind(uuid, file_path, RecordingKind::Live)
}

/// Pause/resume are VOD/Series-only, so those tests need a resumable task.
fn make_test_transfer_task(uuid: &str, file_path: PathBuf) -> RecordingTask {
    make_test_task_of_kind(uuid, file_path, RecordingKind::Vod)
}

fn make_test_task_of_kind(uuid: &str, file_path: PathBuf, kind: RecordingKind) -> RecordingTask {
    let mut task = task(uuid, kind, RecordingTaskState::Running);
    task.file_dir = file_path.parent().unwrap_or(Path::new("/")).to_path_buf();
    task.file_path = file_path;
    task.filename = format!("{uuid}.ts");
    task
}

/// Read back what the repository actually committed, rather than
/// trusting the in-memory mirror.
async fn committed(queue: &RecordingQueue) -> (u64, Vec<String>) {
    let repository = queue.repository.clone().expect("queue is repository backed");
    let snapshot = tokio::task::spawn_blocking(move || {
        let mut guard = repository.lock().expect("repository lock");
        guard.load()
    })
    .await
    .expect("join")
    .expect("load");
    (snapshot.queue_revision, snapshot.tasks.iter().map(|task| task.uuid.clone()).collect())
}

// --- Filename rendering + collision reservation ---

fn collect_existing_relative_paths(candidate: &PersistedRecordingQueue) -> Vec<String> {
    let mut out = Vec::new();
    for d in &candidate.queue {
        if let Some(p) = &d.recording.relative_path {
            out.push(p.clone());
        }
    }
    for d in &candidate.scheduled {
        if let Some(p) = &d.recording.relative_path {
            out.push(p.clone());
        }
    }
    for d in &candidate.active {
        if let Some(p) = &d.recording.relative_path {
            out.push(p.clone());
        }
    }
    for d in &candidate.finished {
        if let Some(p) = &d.recording.relative_path {
            out.push(p.clone());
        }
    }
    out
}

/// Reserve a unique relative path for a new recording inside the
/// queue mutation boundary. The candidate is the in-memory
/// `PersistedRecordingQueue` the closure is building; the helper
/// collects all already-reserved `relative_path` values, applies
/// the supplied stem, and appends a numbered collision suffix
/// (`_1`, `_2`, …) until the result is unique. The reserved path
/// is also written to the recording metadata so a later collision
/// created externally is detected by the worker at execute time.
fn reserve_recording_relative_path(
    candidate: &mut PersistedRecordingQueue,
    stem: &str,
    recording_uuid: &str,
) -> String {
    // If this recording already has a reserved path (e.g., a retry
    // or an edit), keep it. Re-reserving on a re-entrant call must
    // not bump the suffix because of the caller's own previous
    // entry.
    let existing_self = find_recording_relative_path(candidate, recording_uuid);
    if let Some(prior) = existing_self {
        return prior;
    }
    let existing = collect_existing_relative_paths(candidate);
    let reserved = shared::utils::next_collision_suffix(stem, &existing);
    for d in &mut candidate.queue {
        if d.uuid == recording_uuid {
            d.recording.relative_path = Some(reserved.clone());
        }
    }
    for d in &mut candidate.scheduled {
        if d.uuid == recording_uuid {
            d.recording.relative_path = Some(reserved.clone());
        }
    }
    if let Some(d) = candidate.active.iter_mut().find(|task| task.uuid == recording_uuid) {
        d.recording.relative_path = Some(reserved.clone());
    }
    for d in &mut candidate.finished {
        if d.uuid == recording_uuid {
            d.recording.relative_path = Some(reserved.clone());
        }
    }
    reserved
}

fn find_recording_relative_path(candidate: &PersistedRecordingQueue, recording_uuid: &str) -> Option<String> {
    for d in &candidate.queue {
        if d.uuid == recording_uuid {
            return d.recording.relative_path.clone();
        }
    }
    for d in &candidate.scheduled {
        if d.uuid == recording_uuid {
            return d.recording.relative_path.clone();
        }
    }
    for d in &candidate.active {
        if d.uuid == recording_uuid {
            return d.recording.relative_path.clone();
        }
    }
    for d in &candidate.finished {
        if d.uuid == recording_uuid {
            return d.recording.relative_path.clone();
        }
    }
    None
}

fn persisted_recording(uuid: &str, meta: RecordingMetadata) -> PersistedRecordingTask {
    PersistedRecordingTask {
        media_identity: String::new(),
        partition: RecordingPartition::default(),
        uuid: uuid.to_string(),
        kind: RecordingKind::Live,
        file_dir: PathBuf::from("/tmp"),
        file_path: PathBuf::from(format!("/tmp/{uuid}.ts")),
        filename: format!("{uuid}.ts"),
        url: format!("https://example.com/{uuid}"),
        finished: false,
        size: 0,
        total_size: None,
        paused: false,
        error: None,
        state: RecordingTaskState::Scheduled,
        input_name: None,
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: meta,
    }
}

mod admission;
mod lifecycle;
mod policy;
mod publication;
mod recovery;
mod storage;
mod streaming;
mod transport;
