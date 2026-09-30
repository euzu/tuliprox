//! One file, two users, across restarts.
//!
//! The unit tests prove each rule in isolation against a hand-built candidate.
//! This walks the whole thing through the real repository: two library entries
//! resolve to one recording, survive a restart, and the bytes leave exactly
//! when the last entry does. Every defect in this area so far was a subsystem
//! that had never seen a shared file, so the point here is coverage of the
//! seams rather than of any single rule.

use shared::model::{RecordingKind, RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility, UserId};
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tuliprox_dvr::recording::{
    recording_deletion::{begin_deletion, execute_deletion_target, finalize_deletion, DeletionError},
    recording_queue::{
        mutate, PersistedRecordingTask, RecordingControl, RecordingPartition, RecordingQueue, RecordingTaskState,
    },
    recording_worker::recording_partial_path,
};

/// A completed entry for the same programme, owned by `user`.
///
/// The programme identity is what makes two entries share a file, so every
/// field feeding it is deliberately identical between callers.
fn entry_for(user: &str, file: &Path) -> PersistedRecordingTask {
    let meta = RecordingMetadata::new_live(
        RecordingOwner::User(UserId::from(format!("web:{user}").as_str())),
        RecordingVisibility::Private,
        RecordingSource::new("target-1", "virtual-1", "input-1"),
        1_700_000_000,
        1_700_003_600,
        0,
        0,
    );
    PersistedRecordingTask {
        media_identity: String::new(),
        partition: RecordingPartition::default(),
        uuid: format!("{user}-entry"),
        kind: RecordingKind::Live,
        file_dir: file.parent().expect("parent").to_path_buf(),
        file_path: file.to_path_buf(),
        filename: "programme.ts".to_string(),
        url: "https://example.com/programme".to_string(),
        finished: true,
        size: 4_096,
        total_size: Some(4_096),
        paused: false,
        error: None,
        state: RecordingTaskState::Completed,
        input_name: None,
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: meta,
    }
}

/// Round-trip through `RecordingTask` so `media_identity` is computed the way
/// the running server computes it, rather than asserted by the test.
fn seeded(user: &str, file: &Path) -> PersistedRecordingTask {
    let task = RecordingQueue::from_persisted(entry_for(user, file)).expect("valid fixture");
    RecordingQueue::to_persisted(&task)
}

fn identity_of(task: &tuliprox_dvr::recording::recording_queue::RecordingTask) -> String {
    RecordingQueue::to_persisted(task).media_identity
}

/// Open the queue the way a restart does: construct it, then load what the
/// previous process committed.
async fn open_queue(storage: &Path) -> RecordingQueue {
    let queue = RecordingQueue::new_persistent(storage, storage).expect("open recording repository");
    queue.load_from_disk().await.expect("load persisted state");
    queue
}

async fn finished_uuids(queue: &RecordingQueue) -> Vec<String> {
    let mut uuids: Vec<String> = queue.finished.read().await.iter().map(|task| task.uuid.clone()).collect();
    uuids.sort();
    uuids
}

/// Delete one entry the way the service does, reporting whether bytes left.
async fn delete_entry(queue: &RecordingQueue, uuid: &str) -> bool {
    let target = begin_deletion(queue, uuid).await.expect("begin deletion");
    let unlinked = execute_deletion_target(&target).await.expect("execute deletion");
    finalize_deletion(queue, uuid).await.expect("finalize deletion");
    unlinked.is_some()
}

#[tokio::test]
async fn one_recording_serves_two_users_across_restarts_and_leaves_with_the_last_of_them() {
    let dir = TempDir::new().expect("tempdir");
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).expect("storage dir");
    let recording: PathBuf = dir.path().join("programme.ts");
    std::fs::write(&recording, b"the recorded broadcast").expect("write recording");

    // Alice and Bob each ask for the same programme.
    {
        let queue = open_queue(&storage).await;
        for user in ["alice", "bob"] {
            let entry = seeded(user, &recording);
            mutate(&queue, move |candidate| {
                candidate.finished.push(entry.clone());
                Ok(())
            })
            .await
            .expect("seed entry");
        }
        let seeded_entries = queue.finished.read().await;
        assert_eq!(seeded_entries.len(), 2, "two entries");
        assert_eq!(
            identity_of(&seeded_entries[0]),
            identity_of(&seeded_entries[1]),
            "the same programme must resolve to one media identity"
        );
        assert_eq!(seeded_entries[0].file_path, seeded_entries[1].file_path, "and therefore to one file");
    }

    // Restart: the sharing has to survive the repository round trip, not just
    // live in the in-memory candidate.
    {
        let queue = open_queue(&storage).await;
        assert_eq!(finished_uuids(&queue).await, vec!["alice-entry", "bob-entry"]);
        let reloaded = queue.finished.read().await;
        assert_eq!(
            identity_of(&reloaded[0]),
            identity_of(&reloaded[1]),
            "identity must be rebuilt on load, not lost with the process"
        );
    }

    // Alice removes hers. Her entry goes; the bytes do not, because Bob's
    // entry is still pointing at them.
    {
        let queue = open_queue(&storage).await;
        let freed = delete_entry(&queue, "alice-entry").await;
        assert!(!freed, "no space is reclaimed while another entry holds the file");
        assert!(recording.exists(), "Bob's recording must survive Alice's deletion");
        assert_eq!(finished_uuids(&queue).await, vec!["bob-entry"]);
    }

    // Restart again: the deletion is durable and Bob is untouched.
    {
        let queue = open_queue(&storage).await;
        assert_eq!(finished_uuids(&queue).await, vec!["bob-entry"]);
        assert!(recording.exists());
    }

    // Bob removes his. He is the last holder, so the bytes go with him.
    {
        let queue = open_queue(&storage).await;
        let freed = delete_entry(&queue, "bob-entry").await;
        assert!(freed, "the last entry reclaims the space");
        assert!(!recording.exists(), "the file leaves with the last entry");
        assert!(finished_uuids(&queue).await.is_empty(), "both entries are gone");
    }

    // And that is durable too.
    {
        let queue = open_queue(&storage).await;
        assert!(finished_uuids(&queue).await.is_empty(), "the removals survived the restart");
    }
}

#[tokio::test]
async fn two_users_recording_different_programmes_never_share_a_file() {
    // The counterpart to the test above: identity must be specific enough that
    // unrelated recordings are not merged into one file.
    let dir = TempDir::new().expect("tempdir");
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).expect("storage dir");

    let alice_file = dir.path().join("alice.ts");
    let bob_file = dir.path().join("bob.ts");
    std::fs::write(&alice_file, b"alice").expect("write");
    std::fs::write(&bob_file, b"bob").expect("write");

    let queue = open_queue(&storage).await;
    let alice = seeded("alice", &alice_file);
    let mut bob = entry_for("bob", &bob_file);
    bob.recording.source = RecordingSource::new("target-1", "virtual-2", "input-1");
    let bob = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(bob).expect("valid fixture"));
    assert_ne!(alice.media_identity, bob.media_identity, "different programmes are different media");

    for entry in [alice, bob] {
        mutate(&queue, move |candidate| {
            candidate.finished.push(entry.clone());
            Ok(())
        })
        .await
        .expect("seed entry");
    }

    let freed = delete_entry(&queue, "alice-entry").await;
    assert!(freed, "nothing else holds Alice's file");
    assert!(!alice_file.exists());
    assert!(bob_file.exists(), "Bob's unrelated recording is untouched");
}

/// Seed one entry per user, all on the same media, with `active_user` running.
async fn seed_sharing(queue: &RecordingQueue, file: &Path, users: &[&str], active_user: &str) {
    for user in users {
        let mut entry = seeded(user, file);
        if *user == active_user {
            entry.state = RecordingTaskState::Running;
            entry.finished = false;
        }
        let is_active = *user == active_user;
        mutate(queue, move |candidate| {
            if is_active {
                candidate.active = Some(entry.clone());
            } else {
                candidate.queue.push(entry.clone());
            }
            Ok(())
        })
        .await
        .expect("seed entry");
    }
}

#[tokio::test]
async fn cancelling_one_user_leaves_the_other_holding_the_file_across_a_restart() {
    // Task 11: A leaves, B keeps the recording. Cancelling the running entry
    // does not finish it -- the worker still owns the file -- so the entry sits
    // in `Cancelling` until something acknowledges. Here that is a restart,
    // which is also the only acknowledgement available after a crash.
    let dir = TempDir::new().expect("tempdir");
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).expect("storage dir");
    let recording = dir.path().join("programme.ts");
    let partial = recording_partial_path(&recording);
    std::fs::write(&partial, b"bytes in flight").expect("write partial");

    {
        let queue = open_queue(&storage).await;
        seed_sharing(&queue, &recording, &["alice", "bob"], "alice").await;

        let was_paused = queue.cancel_requested("alice-entry").await.expect("cancel");
        assert_eq!(was_paused, Some(false), "the active entry was cancelled");
        assert_eq!(*queue.control_signal.read().await, RecordingControl::Cancel, "the worker is signalled once");
        let active = queue.active.read().await.clone().expect("still active");
        assert_eq!(active.state, RecordingTaskState::Cancelling, "the worker has not let go yet");

        // Removing it now would unlink the partial the worker is still writing.
        assert!(
            matches!(begin_deletion(&queue, "alice-entry").await, Err(DeletionError::NotTerminal)),
            "a recording still being torn down cannot be deleted"
        );
    }

    {
        // The worker died with the process, so nothing will ever acknowledge.
        // Recovery settles it rather than restarting cancelled work.
        let queue = open_queue(&storage).await;
        let all = queue.committed_snapshot().await.1;
        for task in &all {
            eprintln!("DBG {} state={:?} finished={}", task.uuid, task.state, task.finished);
        }
        let alice = all.into_iter().find(|task| task.uuid == "alice-entry").expect("alice survived the restart");
        assert_eq!(alice.state, RecordingTaskState::Cancelled, "cancelling resolves, it does not resume");

        let target = begin_deletion(&queue, "alice-entry").await.expect("now removable");
        assert!(target.still_referenced, "Bob's entry still holds this media");
        execute_deletion_target(&target).await.expect("execute");
        finalize_deletion(&queue, "alice-entry").await.expect("finalize");
        assert!(partial.exists(), "the file Bob is waiting on survives Alice leaving");
    }

    {
        let queue = open_queue(&storage).await;
        let (_revision, tasks) = queue.committed_snapshot().await;
        assert_eq!(tasks.len(), 1, "only Bob's entry remains");
        assert_eq!(tasks[0].uuid, "bob-entry");
        assert!(partial.exists());
    }
}

#[tokio::test]
async fn cancelling_the_last_entry_leaves_nothing_referencing_the_file() {
    // The counterpart: with no other entry attached, the work is genuinely over
    // once the cancellation settles, and the staged bytes go with it.
    let dir = TempDir::new().expect("tempdir");
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).expect("storage dir");
    let recording = dir.path().join("programme.ts");
    let partial = recording_partial_path(&recording);
    std::fs::write(&partial, b"abandoned bytes").expect("write partial");

    {
        let queue = open_queue(&storage).await;
        seed_sharing(&queue, &recording, &["alice"], "alice").await;
        queue.cancel_requested("alice-entry").await.expect("cancel");
        assert!(partial.exists(), "the worker still owns the file while cancelling");
    }

    {
        let queue = open_queue(&storage).await;
        let target = begin_deletion(&queue, "alice-entry").await.expect("begin");
        assert!(!target.still_referenced, "nothing else holds it");
        execute_deletion_target(&target).await.expect("execute");
        finalize_deletion(&queue, "alice-entry").await.expect("finalize");
        assert!(!partial.exists(), "the last entry takes its staged bytes with it");
    }

    {
        let queue = open_queue(&storage).await;
        let (_revision, tasks) = queue.committed_snapshot().await;
        assert!(tasks.is_empty(), "the library is empty and still loads");
    }
}

#[tokio::test]
async fn a_cancelled_recording_stops_holding_the_space_it_reserved() {
    // A reservation is a claim on disk for bytes still to be written. Once the
    // recording is over, nothing more will be written, so keeping the claim
    // charges the user for space no recording occupies -- and the worker paths
    // used to keep it while only the user-initiated cancel released it.
    let dir = TempDir::new().expect("tempdir");
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).expect("storage dir");
    let recording = dir.path().join("programme.ts");

    let queue = open_queue(&storage).await;
    let mut running = seeded("alice", &recording);
    running.state = RecordingTaskState::Running;
    running.finished = false;
    running.recording.reserved_bytes = 5_000;
    mutate(&queue, move |candidate| {
        candidate.active = Some(running.clone());
        Ok(())
    })
    .await
    .expect("seed");

    queue.cancel_requested("alice-entry").await.expect("cancel");

    // The worker settles it; a restart stands in for that here.
    drop(queue);
    let queue = open_queue(&storage).await;
    let settled = queue
        .committed_snapshot()
        .await
        .1
        .into_iter()
        .find(|task| task.uuid == "alice-entry")
        .expect("the entry survived");
    assert_eq!(settled.state, RecordingTaskState::Cancelled);
    assert_eq!(settled.recording.reserved_bytes, 0, "a finished recording reserves nothing");
}
