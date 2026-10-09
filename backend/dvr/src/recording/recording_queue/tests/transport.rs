use super::*;

#[tokio::test]
async fn promotion_precheck_keeps_attachments_and_live_tasks_eligible_while_vod_is_running() {
    let queue = RecordingQueue::new();
    let active = task("active", RecordingKind::Vod, RecordingTaskState::Running);
    let mut duplicate = task("duplicate", RecordingKind::Vod, RecordingTaskState::Queued);
    duplicate.url = active.url.clone();
    let completed = task("completed", RecordingKind::Series, RecordingTaskState::Completed);
    let mut attached = task("attached", RecordingKind::Series, RecordingTaskState::Queued);
    attached.url = completed.url.clone();
    queue.active.write().await.push(active);
    queue.finished.write().await.push(completed);
    queue.queue.lock().await.extend([duplicate, attached]);
    assert!(queue.has_promotable_queued().await);
    mutate(&queue, |candidate| {
        assert!(promote_from_queue(candidate).is_none());
        Ok(())
    })
    .await
    .expect("attach without starting a second transfer");
    assert_eq!(queue.finished.read().await.len(), 2);
    assert!(!queue.has_promotable_queued().await, "the duplicate waits for its active transfer");
    queue.queue.lock().await.push_back(task("live", RecordingKind::Live, RecordingTaskState::Queued));
    assert!(queue.has_promotable_queued().await);
    mutate(&queue, |candidate| {
        assert_eq!(promote_from_queue(candidate).map(|(uuid, _)| uuid).as_deref(), Some("live"));
        Ok(())
    })
    .await
    .expect("start live alongside the transfer");
    assert_eq!(queue.active.read().await.len(), 2);
    assert_eq!(queue.queue.lock().await.len(), 1);
}

#[test]
fn a_queued_entry_runs_when_nothing_else_holds_its_media() {
    let candidate = PersistedRecordingQueue::default();
    let queued = identified("a", "film-42", RecordingTaskState::Queued);
    assert_eq!(promotion_decision(&candidate, &queued), PromotionDecision::Execute);
}

#[test]
fn a_queued_entry_waits_while_another_entry_produces_the_file() {
    // Promoting it would start a second transfer to the same path.
    let candidate = PersistedRecordingQueue {
        active: vec![identified("a", "film-42", RecordingTaskState::Running)],
        ..PersistedRecordingQueue::default()
    };
    let queued = identified("b", "film-42", RecordingTaskState::Queued);
    assert_eq!(promotion_decision(&candidate, &queued), PromotionDecision::Wait);
}

#[test]
fn attaching_adopts_the_file_but_keeps_the_entry_its_own() {
    let mut source = identified("a", "film-42", RecordingTaskState::Completed);
    source.file_path = PathBuf::from("/rec/film.mp4");
    source.filename = "film.mp4".to_string();
    source.size = 4_096;
    source.recording.measured_bytes = 4_096;
    source.recording.completed_at = Some(1_700_000_000);
    source.recording.relative_path = Some("film.mp4".to_string());

    let mut attached = identified("b", "film-42", RecordingTaskState::Queued);
    attached.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
    attached.recording.reserved_bytes = 9_999;
    attach_to_completed(&mut attached, &source);

    // The bytes are shared.
    assert_eq!(attached.file_path, source.file_path);
    assert_eq!(attached.recording.measured_bytes, 4_096);
    assert_eq!(attached.state, RecordingTaskState::Completed);
    // The link is not: this is Bob's entry onto Alice's file.
    assert_eq!(attached.uuid, "b");
    assert_eq!(attached.recording.owner_id().0, "web:bob");
    // Nothing is reserved any more; the bytes are already on disk.
    assert_eq!(attached.recording.reserved_bytes, 0);
}

#[test]
fn organized_live_recording_lands_in_its_group() {
    let mut meta = live_meta("web:alice", 1_700_000_000, 1_800);
    meta.channel_name = Some("SBS 6 HD".to_string());
    meta.group = Some("NEDERLAND".to_string());
    let task = organized_task(RecordingKind::Live, "news.ts", meta);
    assert_eq!(task.recording.relative_path.as_deref(), Some("NEDERLAND/news.ts"));
}
