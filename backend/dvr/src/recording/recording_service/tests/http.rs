use super::*;

#[tokio::test]
async fn unsupported_range_requires_confirmation_before_vod_or_series_partial_is_discarded() {
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(RecordingQueue::new_persistent(dir.path(), dir.path()).expect("recording repository"));
        let mut task =
            persisted_media("recording", "web:alice", RecordingVisibility::Private, "http://provider/film.mp4");
        task.kind = kind;
        task.file_dir = dir.path().to_path_buf();
        task.file_path = dir.path().join("recording.mp4");
        task.state = RecordingTaskState::Failed;
        task.finished = true;
        task.size = 4;
        task.total_size = Some(10);
        task.error = Some(super::super::super::recording_transfer::RANGE_UNSUPPORTED_ERROR.to_string());
        task.recording.resume_etag = Some("\"old-etag\"".to_string());
        let partial = crate::recording::recording_worker::recording_partial_path(&task.file_path);
        std::fs::write(&partial, b"0123").expect("saved partial");
        mutate(&queue, move |candidate| {
            candidate.finished.push(task.clone());
            Ok(())
        })
        .await
        .expect("seed failed recording");
        let service = RecordingService::new(Arc::clone(&queue), test_app_config());
        let claims = shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: Permission::RecordingManage.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from("web:alice")),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        };

        assert!(matches!(service.retry_recording(&claims, "recording").await, Err(ServiceError::InvalidState)));
        assert_eq!(std::fs::read(&partial).expect("partial kept without consent"), b"0123");
        assert!(queue.finished.read().await[0].to_view(true).restart_from_beginning_required);

        assert!(service.restart_recording(&claims, "recording").await.expect("confirmed restart"));
        assert!(!partial.exists());
        assert!(queue.finished.read().await.is_empty());
        let queued = queue.queue.lock().await.front().cloned().expect("queued transfer");
        assert_eq!(queued.state, RecordingTaskState::Queued);
        assert_eq!(queued.size, 0);
        assert_eq!(queued.total_size, None);
        assert_eq!(queued.recording.resume_etag, None);
    }
}
