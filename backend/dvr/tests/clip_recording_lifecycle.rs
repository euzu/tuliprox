//! Tests for F11: clip recordings served from an enclosing live recording.

use shared::model::{
    RecordingContainerFormat, RecordingKind, RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility,
    UserId,
};
use std::path::Path;
use tempfile::TempDir;
use tuliprox_core::model::RecordingConfig;
use tuliprox_dvr::recording::{
    recording_deletion::{begin_deletion, execute_deletion_target, finalize_deletion, maybe_compact_clips},
    recording_queue::{
        mutate, PersistedRecordingQueue, PersistedRecordingTask, RecordingPartition, RecordingQueue, RecordingTaskState,
    },
    recording_service::{find_enclosing_materialization, same_live_source},
    recording_sidecar::{append_time_index_entry, time_index_path},
    recording_transfer::fail_active_download,
    recording_worker::{compute_clip_offsets_sync, read_clip_header, CLIP_HEADER_LEN},
};

fn make_ts_packet_sdt() -> Vec<u8> {
    let mut pkt = vec![0xFF; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40; // payload unit start indicator
    pkt[2] = 0x11; // PID 0x0011 (SDT)
    pkt[3] = 0x10; // payload only
    pkt[4] = 0x00; // pointer field
    pkt[5] = 0x42; // table id SDT
    pkt[6] = 0xF0;
    pkt[7] = 0x0D; // section len 13
    pkt[8] = 0x00;
    pkt[9] = 0x01;
    pkt
}

fn make_ts_packet_pat() -> Vec<u8> {
    let mut pkt = vec![0xFF; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40;
    pkt[2] = 0x00;
    pkt[3] = 0x10;
    pkt[4] = 0x00; // pointer field
    pkt[5] = 0x00; // table id PAT
    pkt[6] = 0xB0;
    pkt[7] = 0x0D; // section len 13
    pkt[8] = 0x00;
    pkt[9] = 0x01;
    pkt[10] = 0xC1;
    pkt[11] = 0x00;
    pkt[12] = 0x00;
    // Program 1 -> PMT PID 0x1000
    pkt[13] = 0x00;
    pkt[14] = 0x01;
    pkt[15] = 0xF0;
    pkt[16] = 0x00;
    // CRC
    pkt[17] = 0x00;
    pkt[18] = 0x00;
    pkt[19] = 0x00;
    pkt[20] = 0x00;
    pkt
}

fn make_ts_packet_pmt() -> Vec<u8> {
    let mut pkt = vec![0xFF; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x50; // PID 0x1000
    pkt[2] = 0x00;
    pkt[3] = 0x10;
    pkt[4] = 0x00; // pointer field
    pkt[5] = 0x02; // table id PMT
    pkt[6] = 0xB0;
    pkt[7] = 0x12; // section len 18
    pkt[8] = 0x00;
    pkt[9] = 0x01;
    pkt[10] = 0xC1;
    pkt[11] = 0x00;
    pkt[12] = 0x00;
    pkt[13] = 0xE1;
    pkt[14] = 0x00; // PCR PID 0x100
    pkt[15] = 0xF0;
    pkt[16] = 0x00; // program info len = 0
                    // Elementary stream 1: H.264 (0x1B) on PID 0x100
    pkt[17] = 0x1B;
    pkt[18] = 0xE1;
    pkt[19] = 0x00; // ES PID 0x100
    pkt[20] = 0xF0;
    pkt[21] = 0x00; // ES info len = 0
                    // CRC
    pkt[22] = 0x00;
    pkt[23] = 0x00;
    pkt[24] = 0x00;
    pkt[25] = 0x00;
    pkt
}

fn make_ts_packet_video(pid: u16, is_idr: bool) -> Vec<u8> {
    let mut pkt = vec![0x00; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | ((pid >> 8) as u8);
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10;
    // PES header (9 bytes)
    pkt[4] = 0x00;
    pkt[5] = 0x00;
    pkt[6] = 0x01;
    pkt[7] = 0xE0; // Video stream
    pkt[8] = 0x00;
    pkt[9] = 0x00;
    pkt[10] = 0x80;
    pkt[11] = 0x00;
    pkt[12] = 0x00; // header data len = 0
                    // NAL unit start code: 0, 0, 1
    pkt[13] = 0x00;
    pkt[14] = 0x00;
    pkt[15] = 0x01;
    // NAL type: 5 for IDR, 1 for non-IDR
    pkt[16] = if is_idr { 0x05 } else { 0x01 };
    pkt
}

fn make_synthetic_stream() -> Vec<u8> {
    let mut data = Vec::with_capacity(7 * 188);
    data.extend(make_ts_packet_sdt()); // pkt 0: SDT (offset 0)
    data.extend(make_ts_packet_pat()); // pkt 1: PAT (offset 188)
    data.extend(make_ts_packet_pmt()); // pkt 2: PMT (offset 376)
    data.extend(make_ts_packet_video(0x100, false)); // pkt 3: non-IDR (offset 564)
    data.extend(make_ts_packet_video(0x100, true)); // pkt 4: IDR (offset 752)
    data.extend(make_ts_packet_video(0x100, false)); // pkt 5: non-IDR (offset 940)
    data.extend(make_ts_packet_video(0x100, false)); // pkt 6: non-IDR (offset 1128)
    data
}

fn create_test_recording(
    uuid: &str,
    user: &str,
    file_path: &Path,
    start_ts: i64,
    end_ts: i64,
    state: RecordingTaskState,
) -> PersistedRecordingTask {
    let mut meta = RecordingMetadata::new_live(
        RecordingOwner::User(UserId::from(format!("web:{user}").as_str())),
        RecordingVisibility::Private,
        RecordingSource::new("target-1", "virtual-1", "input-1").with_provider_id(Some("stream-1".to_string())),
        start_ts,
        end_ts,
        0,
        0,
    );
    meta.scheduled_start = Some(start_ts);
    meta.scheduled_end = Some(end_ts);
    meta.reserved_bytes = 10_000;

    let task = PersistedRecordingTask {
        media_identity: String::new(),
        partition: RecordingPartition::default(),
        uuid: uuid.to_string(),
        vod_id: 0,
        kind: RecordingKind::Live,
        file_dir: file_path.parent().unwrap().to_path_buf(),
        file_path: file_path.to_path_buf(),
        filename: "enclosing.ts".to_string(),
        url: "https://example.com/live".to_string(),
        finished: matches!(
            state,
            RecordingTaskState::Completed | RecordingTaskState::Failed | RecordingTaskState::Cancelled
        ),
        size: 0,
        total_size: None,
        paused: false,
        error: None,
        state,
        input_name: Some("input-1".to_string()),
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: meta,
        clip: None,
        cancelled_by_owner: false,
    };
    let runtime = RecordingQueue::from_persisted(task).unwrap();
    RecordingQueue::to_persisted(&runtime)
}

#[tokio::test]
async fn clip_attaches_to_enclosing_live_recording_without_second_materialization() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("enclosing.ts");

    let mut rec_cfg = RecordingConfig::from(&shared::model::RecordingConfigDto::default());
    rec_cfg.container_format = RecordingContainerFormat::Mpegts;

    // A: 20:00 - 22:00 (unix 2000..8200) running
    let mut queue = PersistedRecordingQueue::default();
    let task_a = create_test_recording("task-a", "alice", &file, 2000, 8200, RecordingTaskState::Running);
    queue.active.push(task_a.clone());

    // Request B: 20:30 - 21:00 (unix 3800..5600)
    let task_b = create_test_recording("task-b", "bob", &file, 3800, 5600, RecordingTaskState::Scheduled);

    // Verify same_live_source matches
    assert!(same_live_source(&task_a.recording, &task_a.url, &task_b.recording, &task_b.url));

    // Verify find_enclosing_materialization selects A
    let enclosing = find_enclosing_materialization(&queue, &task_b);
    assert!(enclosing.is_some());
    let enclosing = enclosing.unwrap();
    assert_eq!(enclosing.uuid, "task-a");
}

#[tokio::test]
async fn delivered_clip_starts_with_pat_pmt_and_idr() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("broadcast.ts");
    // Synthetic stream has SDT at packet 0, PAT at packet 1, PMT at packet 2
    let synthetic_ts = make_synthetic_stream();
    std::fs::write(&file_path, &synthetic_ts).unwrap();

    let idx = time_index_path(&file_path);
    append_time_index_entry(&idx, 1000, 0).await.unwrap();
    append_time_index_entry(&idx, 1010, 564).await.unwrap();
    append_time_index_entry(&idx, 1020, 1316).await.unwrap();

    // Clip from 1005 to 1015
    let (start_offset, end_offset) = compute_clip_offsets_sync(&file_path, 1005, 1015).unwrap();

    // Start offset must scan past non-IDR packet 3 to packet 4 (IDR start) = 752
    assert_eq!(start_offset, 752);
    assert_eq!(end_offset, 1316);

    // Finding 1: read_clip_header returns PAT + PMT (376 bytes), skipping SDT
    let header = read_clip_header(&file_path).await.unwrap();
    assert_eq!(header.len(), CLIP_HEADER_LEN);
    // Packet 0 of header is PAT (PID 0)
    assert_eq!(header[0], 0x47);
    assert_eq!(header[1] & 0x1F, 0x00);
    assert_eq!(header[2], 0x00);
    // Packet 1 of header is PMT (PID 0x1000)
    assert_eq!(header[188], 0x47);
    assert_eq!(header[189] & 0x1F, 0x10);
    assert_eq!(header[190], 0x00);

    // Payload start offset points to IDR
    let start_idx = usize::try_from(start_offset).unwrap_or(0);
    let first_payload_pkt = &synthetic_ts[start_idx..start_idx + 188];
    assert_eq!(first_payload_pkt[0], 0x47);
    assert_eq!(first_payload_pkt[16], 0x05);
}

#[tokio::test]
async fn clip_completes_while_enclosing_recording_writes_partial_file() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let final_path = dir.path().join("enclosing.ts");
    let partial_path = tuliprox_dvr::recording_worker::recording_partial_path(&final_path);

    // Write data to the partial file only; final file does not exist yet
    let synthetic_ts = make_synthetic_stream();
    std::fs::write(&partial_path, &synthetic_ts).unwrap();
    assert!(!final_path.exists());

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    let idx = time_index_path(&final_path);
    append_time_index_entry(&idx, 1000, 0).await.unwrap();
    append_time_index_entry(&idx, 1010, 564).await.unwrap();
    append_time_index_entry(&idx, 1020, 1316).await.unwrap();

    // A is active running
    let task_a = create_test_recording("task-a", "alice", &final_path, 1000, 2000, RecordingTaskState::Running);
    // B is clip on A
    let mut task_b = create_test_recording("task-b", "bob", &final_path, 1005, 1015, RecordingTaskState::Scheduled);
    task_b.media_identity = task_a.media_identity.clone();
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1005,
        end_ts: 1015,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    mutate(&queue, |candidate| {
        candidate.active.push(task_a.clone());
        candidate.scheduled.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Promote due at 1020 (index has passed 1015)
    queue.promote_due_scheduled(1020).await;

    let (_rev, tasks) = queue.committed_snapshot().await;
    let finished_b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(finished_b.state, RecordingTaskState::Completed);
    let clip = finished_b.clip.as_ref().unwrap();
    assert_eq!(clip.start_offset, Some(752));
    assert_eq!(clip.end_offset, Some(1316));
}

#[tokio::test]
async fn clip_fails_when_enclosing_entry_deleted_and_window_elapsed() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("enclosing.ts");

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    // B is clip whose enclosing entry never existed or was deleted
    let mut task_b = create_test_recording("task-b", "bob", &file_path, 1000, 1020, RecordingTaskState::Scheduled);
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1000,
        end_ts: 1020,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    mutate(&queue, |candidate| {
        candidate.scheduled.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Window has elapsed (now = 1030 >= 1020) and enclosing is gone
    queue.promote_due_scheduled(1030).await;

    let (_rev, tasks) = queue.committed_snapshot().await;
    let finished_b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(finished_b.state, RecordingTaskState::Failed);
    assert_eq!(finished_b.error.as_deref(), Some("recording_source_recording_failed"));
}

#[tokio::test]
async fn clip_fails_with_source_gap_when_enclosing_recording_has_gap() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("enclosing.ts");

    let synthetic_ts = make_synthetic_stream();
    std::fs::write(&file_path, &synthetic_ts).unwrap();

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    let task_a = create_test_recording("task-a", "alice", &file_path, 1000, 2000, RecordingTaskState::Running);
    let mut task_b = create_test_recording("task-b", "bob", &file_path, 1010, 1030, RecordingTaskState::Scheduled);
    task_b.media_identity = task_a.media_identity.clone();
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1010,
        end_ts: 1030,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    mutate(&queue, |candidate| {
        candidate.active.push(task_a.clone());
        candidate.scheduled.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Inject a 20-second gap in the time index across the clip window: 1010 to 1030
    let idx = time_index_path(&file_path);
    append_time_index_entry(&idx, 1005, 0).await.unwrap();
    append_time_index_entry(&idx, 1010, 564).await.unwrap();
    // Big gap between 1010 and 1030 (> 5 seconds)
    append_time_index_entry(&idx, 1030, 1128).await.unwrap();

    queue.promote_due_scheduled(1035).await;

    let (_rev, tasks) = queue.committed_snapshot().await;
    let finished_b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(finished_b.state, RecordingTaskState::Failed);
    assert_eq!(finished_b.error.as_deref(), Some("recording_source_gap"));
}

#[tokio::test]
async fn cancelled_enclosing_recording_continues_work_until_clip_completes() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("enclosing.ts");
    let synthetic_ts = make_synthetic_stream();
    std::fs::write(&file_path, &synthetic_ts).unwrap();

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    let now = chrono::Utc::now().timestamp();
    let start_a = now - 600;
    let end_a = now + 3600;
    let start_b = now - 10;
    let end_b = now + 10;

    // A: active
    let task_a = create_test_recording("task-a", "alice", &file_path, start_a, end_a, RecordingTaskState::Running);
    // B: clip on A
    let mut task_b = create_test_recording("task-b", "bob", &file_path, start_b, end_b, RecordingTaskState::Scheduled);
    task_b.media_identity = task_a.media_identity.clone();
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: start_b,
        end_ts: end_b,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    // C: active task of different media to verify no other active task is touched
    let file_path_c = dir.path().join("other.ts");
    let mut task_c =
        create_test_recording("task-c", "charlie", &file_path_c, start_a, end_a, RecordingTaskState::Running);
    task_c.media_identity = "media-c".to_string();

    mutate(&queue, |candidate| {
        candidate.active.push(task_a.clone());
        candidate.active.push(task_c.clone());
        candidate.scheduled.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Simulate claiming worker for A
    let worker_a = queue.worker("task-a");
    *worker_a.running.write().await = true;

    // Alice cancels A at unix `now` (after B started, but before B ended at `end_b`)
    let cancelled = queue.cancel_requested("task-a").await.unwrap();
    assert_eq!(cancelled, Some(false), "A stays active while B keeps worker alive");

    // Check candidate state:
    // A stays in active under its own uuid, keeps worker, marked cancelled_by_owner, stop time = end_b, reserved = 0
    let (_rev, tasks) = queue.committed_snapshot().await;
    let active_a = tasks.iter().find(|t| t.uuid == "task-a").unwrap();
    assert_eq!(active_a.state, RecordingTaskState::Running);
    assert!(active_a.cancelled_by_owner);
    assert_eq!(active_a.recording.scheduled_end, Some(end_b));
    assert_eq!(active_a.recording.reserved_bytes, 0);
    assert_eq!(tuliprox_dvr::recording_quota::charge_for_task(active_a), 0);

    // B stays in scheduled
    let sched_b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(sched_b.state, RecordingTaskState::Scheduled);

    // C is untouched
    let active_c = tasks.iter().find(|t| t.uuid == "task-c").unwrap();
    assert_eq!(active_c.state, RecordingTaskState::Running);
    assert!(!active_c.cancelled_by_owner);

    // Alice's owner view of A hides A
    let alice_claims = shared::model::Claims {
        username: "alice".to_string(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: 0,
        roles: shared::model::RoleSet::new(),
        permissions: shared::model::Permission::RecordingManage.into(),
        pwd_version: 0,
        subject_id: Some(UserId::from("web:alice")),
        permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
    };
    assert!(active_a.to_owner_view(&alice_claims, true).is_none());

    // Write index entry past B's end_ts
    let idx = time_index_path(&file_path);
    append_time_index_entry(&idx, start_b, 0).await.unwrap();
    append_time_index_entry(&idx, now, 564).await.unwrap();
    append_time_index_entry(&idx, end_b, 1128).await.unwrap();

    // promote_due_scheduled at end_b completes B
    queue.promote_due_scheduled(end_b).await;

    let (_rev, tasks) = queue.committed_snapshot().await;
    let finished_b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(finished_b.state, RecordingTaskState::Completed);
    assert!(finished_b.clip.as_ref().unwrap().start_offset.is_some());
    assert!(finished_b.clip.as_ref().unwrap().end_offset.is_some());

    // When worker stops, A ends as Cancelled and cleanup_settled_cancelled_enclosings
    // removes A from the queue since all attached clips (B) have settled.
    mutate(&queue, |candidate| {
        let a = candidate.active.iter_mut().find(|t| t.uuid == "task-a").unwrap();
        a.finished = true;
        a.state = RecordingTaskState::Cancelled;
        a.error = Some("Cancelled by user".to_string());
        candidate.finished.push(a.clone());
        candidate.active.retain(|t| t.uuid != "task-a");
        Ok(())
    })
    .await
    .unwrap();

    let actions =
        mutate(&queue, |candidate| Ok(RecordingQueue::cleanup_settled_cancelled_enclosings(candidate))).await.unwrap();
    assert_eq!(actions.files_to_compact, vec![file_path.clone()]);
    for f in actions.files_to_compact {
        maybe_compact_clips(&queue, &f).await.unwrap();
    }

    let (_rev, tasks) = queue.committed_snapshot().await;
    // A is gone from the queue
    assert!(tasks.iter().find(|t| t.uuid == "task-a").is_none());
    // B is still present and updated to the compacted file
    let b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(b.state, RecordingTaskState::Completed);
    let compacted_path = dir.path().join("enclosing_compacted.ts");
    assert_eq!(b.file_path, compacted_path);
    assert!(compacted_path.exists());
    assert!(!file_path.exists());

    // Worker releases on completion
    queue.release_worker("task-a").await;

    // Worker map cleaned up, no leaked entries for task-a or task-b
    assert!(queue.existing_worker("task-a").is_none());
    assert!(queue.existing_worker("task-b").is_none());
}

#[tokio::test]
async fn cancel_enclosing_recording_with_no_remaining_clips_deletes_file() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("enclosing_no_clips.ts");
    let synthetic_ts = make_synthetic_stream();
    std::fs::write(&file_path, &synthetic_ts).unwrap();
    let idx = time_index_path(&file_path);
    append_time_index_entry(&idx, 1000, 0).await.unwrap();

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();
    let now = chrono::Utc::now().timestamp();
    let mut task_a =
        create_test_recording("task-a", "alice", &file_path, now - 600, now + 3600, RecordingTaskState::Running);
    task_a.cancelled_by_owner = true;

    mutate(&queue, |candidate| {
        candidate.active.push(task_a.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Worker stops: A moves to finished with cancelled_by_owner
    mutate(&queue, |candidate| {
        let a = candidate.active.iter_mut().find(|t| t.uuid == "task-a").unwrap();
        a.finished = true;
        a.state = RecordingTaskState::Cancelled;
        candidate.finished.push(a.clone());
        candidate.active.retain(|t| t.uuid != "task-a");
        Ok(())
    })
    .await
    .unwrap();

    // With no clips attached, cleanup deletes the file and index
    let actions =
        mutate(&queue, |candidate| Ok(RecordingQueue::cleanup_settled_cancelled_enclosings(candidate))).await.unwrap();
    assert_eq!(actions.files_to_delete, vec![file_path.clone()]);
    for f in actions.files_to_delete {
        tuliprox_dvr::recording_deletion::unlink_recording_file_and_sidecars(&f).await;
    }

    let (_rev, tasks) = queue.committed_snapshot().await;
    assert!(tasks.iter().find(|t| t.uuid == "task-a").is_none());
    assert!(!file_path.exists());
    assert!(!idx.exists());
}

#[tokio::test]
async fn rule_occurrence_and_manual_request_for_same_group_share_materialization() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("enclosing_group.ts");

    let _rec_cfg = RecordingConfig::from(&shared::model::RecordingConfigDto::default());

    let mut queue = PersistedRecordingQueue::default();
    let mut task_rule = create_test_recording("task-rule", "alice", &file, 2000, 8200, RecordingTaskState::Running);
    task_rule.recording.source.is_group = true;
    task_rule.recording.source.virtual_id = "100".to_string();
    task_rule.recording.source.target_id = "target-1".to_string();
    task_rule.recording.program_start = Some(2000);
    task_rule.recording.program_end = Some(8200);
    task_rule.recording.provenance = shared::model::recording::RecordingProvenance {
        rule_id: Some("rule-1".to_string()),
        occurrence_key: Some("occ-1".to_string()),
    };
    task_rule.media_identity =
        tuliprox_dvr::recording_service::recording_identity_key(&task_rule.recording, &task_rule.url);
    queue.active.push(task_rule.clone());

    // Manual request for same group programme
    let mut task_manual = create_test_recording("task-manual", "bob", &file, 2000, 8200, RecordingTaskState::Scheduled);
    task_manual.recording.source.is_group = true;
    task_manual.recording.source.virtual_id = "100".to_string();
    task_manual.recording.source.target_id = "target-1".to_string();
    task_manual.recording.program_start = Some(2000);
    task_manual.recording.program_end = Some(8200);
    task_manual.media_identity =
        tuliprox_dvr::recording_service::recording_identity_key(&task_manual.recording, &task_manual.url);

    // Verify both have identical group identities and share the same materialization ID
    assert_eq!(task_rule.media_identity, task_manual.media_identity);
    assert_eq!(
        tuliprox_repository::recording_repository::materialization_id_for(&task_rule),
        tuliprox_repository::recording_repository::materialization_id_for(&task_manual)
    );
    assert!(same_live_source(&task_rule.recording, &task_rule.url, &task_manual.recording, &task_manual.url));

    // A clip on that same group channel also attaches to the rule materialization
    let mut task_clip = create_test_recording("task-clip", "charlie", &file, 3000, 5000, RecordingTaskState::Scheduled);
    task_clip.recording.source.is_group = true;
    task_clip.recording.source.virtual_id = "100".to_string();
    task_clip.recording.source.target_id = "target-1".to_string();
    let enclosing = find_enclosing_materialization(&queue, &task_clip);
    assert!(enclosing.is_some());
    assert_eq!(enclosing.unwrap().uuid, "task-rule");
}

#[tokio::test]
async fn early_end_of_enclosing_fails_clip_with_source_gap() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("enclosing.ts");

    let synthetic_ts = make_synthetic_stream();
    std::fs::write(&file_path, &synthetic_ts).unwrap();

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    let mut task_a = create_test_recording("task-a", "alice", &file_path, 1000, 1020, RecordingTaskState::Completed);
    task_a.media_identity = "media-1".to_string();

    let mut task_b = create_test_recording("task-b", "bob", &file_path, 1000, 1060, RecordingTaskState::Scheduled);
    task_b.media_identity = task_a.media_identity.clone();
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1000,
        end_ts: 1060,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    mutate(&queue, |candidate| {
        candidate.finished.push(task_a.clone());
        candidate.scheduled.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Time index only reaches 1020 (source ended early before clip's end_ts 1060)
    let idx = time_index_path(&file_path);
    append_time_index_entry(&idx, 1000, 0).await.unwrap();
    append_time_index_entry(&idx, 1020, 564).await.unwrap();

    // Promote due at 1070: index did not reach 1060, and A is terminal -> fails with recording_source_gap
    queue.promote_due_scheduled(1070).await;

    let (_rev, tasks) = queue.committed_snapshot().await;
    let finished_b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(finished_b.state, RecordingTaskState::Failed);
    assert_eq!(finished_b.error.as_deref(), Some("recording_source_gap"));
}

#[tokio::test]
async fn clip_header_bytes_persisted_and_used_on_completed_clip() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("broadcast.ts");

    let synthetic_ts = make_synthetic_stream();
    std::fs::write(&file_path, &synthetic_ts).unwrap();

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    let task_a = create_test_recording("task-a", "alice", &file_path, 1000, 2000, RecordingTaskState::Completed);
    let mut task_b = create_test_recording("task-b", "bob", &file_path, 1005, 1015, RecordingTaskState::Scheduled);
    task_b.media_identity = task_a.media_identity.clone();
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1005,
        end_ts: 1015,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    mutate(&queue, |candidate| {
        candidate.finished.push(task_a.clone());
        candidate.scheduled.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    let idx = time_index_path(&file_path);
    append_time_index_entry(&idx, 1000, 0).await.unwrap();
    append_time_index_entry(&idx, 1010, 564).await.unwrap();
    append_time_index_entry(&idx, 1020, 1316).await.unwrap();

    // promote_due_scheduled completes B and stores header_bytes
    queue.promote_due_scheduled(1025).await;

    let (_rev, tasks) = queue.committed_snapshot().await;
    let b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(b.state, RecordingTaskState::Completed);
    let clip = b.clip.as_ref().unwrap();
    assert!(clip.header_bytes.is_some());
    let header_bytes = clip.header_bytes.as_ref().unwrap();
    assert_eq!(header_bytes.len(), CLIP_HEADER_LEN);
}

#[tokio::test]
async fn failed_enclosing_recording_fails_clip_with_source_recording_failed() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("enclosing.ts");

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    let task_a = create_test_recording("task-a", "alice", &file_path, 2000, 8200, RecordingTaskState::Running);
    let mut task_b = create_test_recording("task-b", "bob", &file_path, 3800, 5600, RecordingTaskState::Scheduled);
    task_b.media_identity = task_a.media_identity.clone();
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 3800,
        end_ts: 5600,
        start_offset: None,
        end_offset: None,
        header_bytes: None,
    });

    mutate(&queue, |candidate| {
        candidate.active.push(task_a.clone());
        candidate.scheduled.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Fail active A at 4000
    fail_active_download(&queue, "task-a", "stream interrupted").await.unwrap();

    let (_rev, tasks) = queue.committed_snapshot().await;
    let finished_b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(finished_b.state, RecordingTaskState::Failed);
    assert_eq!(finished_b.error.as_deref(), Some("recording_source_recording_failed"));
}

#[tokio::test]
async fn quota_of_clip_is_clip_size_not_full_file() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("enclosing.ts");

    let synthetic_ts = make_synthetic_stream();
    std::fs::write(&file_path, &synthetic_ts).unwrap();

    let idx = time_index_path(&file_path);
    append_time_index_entry(&idx, 1000, 0).await.unwrap();
    append_time_index_entry(&idx, 1010, 564).await.unwrap();
    append_time_index_entry(&idx, 1020, 1316).await.unwrap();

    let (start_offset, end_offset) = compute_clip_offsets_sync(&file_path, 1005, 1015).unwrap();
    let clip_size = (end_offset - start_offset) + (CLIP_HEADER_LEN as u64);

    let mut task_b = create_test_recording("task-b", "bob", &file_path, 1005, 1015, RecordingTaskState::Completed);
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1005,
        end_ts: 1015,
        start_offset: Some(start_offset),
        end_offset: Some(end_offset),
        header_bytes: None,
    });
    task_b.size = clip_size;
    task_b.recording.measured_bytes = clip_size;
    task_b.recording.reserved_bytes = 0;

    let charge = tuliprox_dvr::recording_quota::charge_for_task(&task_b);
    assert_eq!(charge, clip_size);
    assert!(charge < u64::try_from(synthetic_ts.len()).unwrap_or(0));
}

#[tokio::test]
async fn compaction_shrinks_file_when_only_clips_remain_and_preserves_content() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("broadcast.ts");

    let mut full_data = make_synthetic_stream();
    full_data.extend(make_ts_packet_video(0x100, false));
    full_data.extend(make_ts_packet_video(0x100, false));
    full_data.extend(make_ts_packet_video(0x100, false));
    std::fs::write(&file_path, &full_data).unwrap();

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    // A is full recording
    let full_len = u64::try_from(full_data.len()).unwrap_or(0);
    let mut task_a = create_test_recording("task-a", "alice", &file_path, 1000, 2000, RecordingTaskState::Completed);
    task_a.size = full_len;
    task_a.recording.measured_bytes = full_len;

    // B is clip covering packet 4 to packet 6: start_offset = 752, end_offset = 1316
    let mut task_b = create_test_recording("task-b", "bob", &file_path, 1200, 1500, RecordingTaskState::Completed);
    task_b.media_identity = task_a.media_identity.clone();
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1200,
        end_ts: 1500,
        start_offset: Some(752),
        end_offset: Some(1316),
        header_bytes: None,
    });
    let original_clip_size = (1316 - 752) + (CLIP_HEADER_LEN as u64);
    task_b.size = original_clip_size;
    task_b.recording.measured_bytes = original_clip_size;

    mutate(&queue, |candidate| {
        candidate.finished.push(task_a.clone());
        candidate.finished.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Expected bytes of B after compaction: PAT (packet 1) + PMT (packet 2) + payload (752..1316)
    let mut expected_bytes: Vec<u8> = Vec::new();
    expected_bytes.extend(&full_data[188..564]); // PAT + PMT (skipping SDT at 0..188)
    expected_bytes.extend(&full_data[752..1316]);

    // Delete A
    let target = begin_deletion(&queue, "task-a").await.unwrap();
    assert!(target.still_referenced, "still referenced by B");
    execute_deletion_target(&target).await.unwrap();
    finalize_deletion(&queue, "task-a").await.unwrap();

    // Compact clips
    let compacted = maybe_compact_clips(&queue, &file_path).await.unwrap();
    assert!(compacted, "compacted successfully");

    // Old file was deleted
    assert!(!file_path.exists());

    // New file exists next to original with _compacted.ts suffix
    let compacted_path = dir.path().join("broadcast_compacted.ts");
    assert!(compacted_path.exists());

    let new_metadata = std::fs::metadata(&compacted_path).unwrap();
    assert_eq!(new_metadata.len(), (1316 - 752) + (CLIP_HEADER_LEN as u64));

    let compacted_data = std::fs::read(&compacted_path).unwrap();
    assert_eq!(compacted_data, expected_bytes);

    // No .compacting staging file remains
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(!name.contains(".compacting"), "staging file must not remain: {name}");
    }

    // Verify task B in queue updated to new path and new offsets
    let (_rev, tasks) = queue.committed_snapshot().await;
    let b = tasks.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(b.file_path, compacted_path);
    assert_eq!(b.clip.as_ref().unwrap().start_offset, Some(CLIP_HEADER_LEN as u64));
    assert_eq!(b.clip.as_ref().unwrap().end_offset, Some((CLIP_HEADER_LEN as u64) + (1316 - 752)));
}

#[tokio::test]
async fn compaction_commit_failure_preserves_old_file_and_offsets() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("broadcast.ts");

    let mut full_data = make_synthetic_stream();
    full_data.extend(make_ts_packet_video(0x100, false));
    std::fs::write(&file_path, &full_data).unwrap();

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    // B is clip covering packet 4 to packet 6: start_offset = 752, end_offset = 1316
    let mut task_b = create_test_recording("task-b", "bob", &file_path, 1200, 1500, RecordingTaskState::Completed);
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1200,
        end_ts: 1500,
        start_offset: Some(752),
        end_offset: Some(1316),
        header_bytes: None,
    });
    task_b.size = (1316 - 752) + (CLIP_HEADER_LEN as u64);

    mutate(&queue, |candidate| {
        candidate.finished.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    // Poison the repository lock so the next mutate inside maybe_compact_clips fails
    let repo = queue.repository.clone().unwrap();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = repo.lock().unwrap();
        panic!("simulated repository lock poison");
    }));

    // Compacting clips fails on commit
    let result = maybe_compact_clips(&queue, &file_path).await;
    assert!(result.is_err());

    // Old file remains intact
    assert!(file_path.exists());
    assert_eq!(std::fs::read(&file_path).unwrap(), full_data);

    // Staged compacted file was cleaned up
    let compacted_path = dir.path().join("broadcast_compacted.ts");
    assert!(!compacted_path.exists());

    // Task B in finished partition still points to old file and old offsets
    let finished = queue.finished.read().await;
    let b = finished.iter().find(|t| t.uuid == "task-b").unwrap();
    assert_eq!(b.file_path, file_path);
    assert_eq!(b.clip.as_ref().unwrap().start_offset, Some(752));
    assert_eq!(b.clip.as_ref().unwrap().end_offset, Some(1316));
}

#[tokio::test]
async fn compaction_write_error_cleans_up_staging_and_leaves_no_compacted_file() {
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("state");
    std::fs::create_dir_all(&storage).unwrap();
    let file_path = dir.path().join("broadcast.ts");

    // File only has synthetic stream (1316 bytes), but clip extends to 2000 bytes (source shorter than clip ranges)
    let full_data = make_synthetic_stream();
    std::fs::write(&file_path, &full_data).unwrap();

    let queue = RecordingQueue::new_persistent(&storage, &storage).unwrap();

    let mut task_b = create_test_recording("task-b", "bob", &file_path, 1200, 1500, RecordingTaskState::Completed);
    task_b.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
        start_ts: 1200,
        end_ts: 1500,
        start_offset: Some(752),
        end_offset: Some(2000),
        header_bytes: None,
    });
    task_b.size = (2000 - 752) + (CLIP_HEADER_LEN as u64);

    mutate(&queue, |candidate| {
        candidate.finished.push(task_b.clone());
        Ok(())
    })
    .await
    .unwrap();

    let result = maybe_compact_clips(&queue, &file_path).await;
    assert!(result.is_err(), "compaction on truncated source must fail: {result:?}");

    let compacted_path = dir.path().join("broadcast_compacted.ts");
    assert!(!compacted_path.exists(), "compacted file must not remain on write failure");

    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(!name.contains(".compacting"), "no staging file must remain, found: {name}");
    }
}
