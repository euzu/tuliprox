use super::*;

#[tokio::test]
async fn edit_recording_clears_epg_when_channel_changes_without_programme() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
    let mut task =
        RecordingQueue::from_persisted(persisted_rule_recording("recording", None, 100)).expect("valid recording task");
    {
        let meta = &mut task.recording;
        meta.channel_id = Some("a".into());
        meta.channel_name = Some("A".into());
        meta.epg = Some(shared::model::recording::EpgEpisodeMetadata {
            programme_id: Some("p-1".into()),
            series_id: None,
            episode_id: None,
            season: None,
            episode: None,
            airing: shared::model::recording::AiringStatus::New,
        });
    }
    downloads.scheduled.write().await.push(task);
    downloads.persist_to_disk().await.expect("persist initial queue");
    let service = RecordingService::new(Arc::clone(&downloads), test_app_config());
    let claims = shared::model::Claims {
        username: "alice".to_string(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: 0,
        roles: shared::model::RoleSet::new(),
        permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
        pwd_version: 0,
        subject_id: Some(UserId::from("web:alice")),
        permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
    };
    let patch = EditRecordingPatch { channel_id: Some("b".into()), ..EditRecordingPatch::default() };

    let result = service.edit_recording(&claims, "recording", patch).await;
    assert!(result.is_ok());
    let scheduled = downloads.scheduled.read().await;
    let meta = &scheduled[0].recording;
    assert_eq!(meta.channel_id.as_deref(), Some("b"));
    assert!(meta.epg.is_none(), "epg metadata must be cleared when channel changed without a fresh programme");
}

#[tokio::test]
async fn preview_conflict_ignores_other_target_or_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_file = dir.path().join("downloads.json");
    let downloads =
        Arc::new(RecordingQueue::new_persistent(&state_file, &state_file).expect("open recording repository"));
    let mut other_target = persisted_rule_recording("other-target", None, 100);
    other_target.recording.source = shared::model::recording::RecordingSource::new("other-target", "9", "input-a");
    let mut other_input = persisted_rule_recording("other-input", None, 100);
    other_input.recording.source = shared::model::recording::RecordingSource::new("1", "9", "input-b");
    let other_target_task = RecordingQueue::from_persisted(other_target).expect("valid task");
    let other_input_task = RecordingQueue::from_persisted(other_input).expect("valid task");
    downloads.queue.lock().await.push_back(other_target_task);
    downloads.queue.lock().await.push_back(other_input_task);
    let points = collect_demand_points_for_provider(&downloads, "1", "input-a").await;
    assert!(points.is_empty(), "foreign target or input must not leak into the demand set");
}
