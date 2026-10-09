use super::*;

fn test_source() -> RecordingSource { RecordingSource::new("target-1", "v-1", "input-a") }

fn test_owner() -> RecordingOwner { RecordingOwner::User(UserId::from("web:abc")) }

#[test]
fn legacy_recording_source_defaults_to_live_cluster() {
    let source: RecordingSource =
        serde_json::from_str(r#"{"target_id":"target","virtual_id":"42","input_name":"input-a"}"#)
            .expect("deserialize source");

    assert_eq!(source.cluster, super::super::XtreamCluster::Live);
}

#[test]
fn recording_kind_serializes_as_snake_case() {
    assert_eq!(serde_json::to_string(&RecordingKind::Vod).expect("serialize"), "\"vod\"");
    assert_eq!(serde_json::to_string(&RecordingKind::Series).expect("serialize"), "\"series\"");
    assert_eq!(serde_json::to_string(&RecordingKind::Live).expect("serialize"), "\"live\"");
}

fn create_request_json() -> &'static str {
    r#"{"source":{"target_id":"t","virtual_id":"42","cluster":"Video","input_name":"in"},"program_title":"Film","visibility":"private"}"#
}

#[test]
fn create_request_round_trips_its_minimal_form() {
    let request: CreateRecordingRequest = serde_json::from_str(create_request_json()).expect("deserialize");
    assert_eq!(request.visibility, RecordingVisibility::Private);
    assert_eq!(request.program_start, None);
    assert_eq!(serde_json::to_string(&request).expect("serialize"), create_request_json());
}

#[test]
fn create_request_rejects_an_unknown_field() {
    // A client that sends a field this build does not understand has a
    // different idea of what it is asking for; recording the wrong thing
    // is worse than refusing.
    let body = r#"{"source":{"target_id":"t","virtual_id":"42","cluster":"Video","input_name":"in"},
            "program_title":"Film","visibility":"private","url":"http://evil/stream.ts"}"#;
    let error = serde_json::from_str::<CreateRecordingRequest>(body).expect_err("unknown field must be rejected");
    assert!(error.to_string().contains("url"), "{error}");
}

#[test]
fn create_request_rejects_an_unknown_visibility() {
    // The visibility is the server's enum on the wire, so a typo in the
    // frontend cannot reach the server as a valid body.
    let body = r#"{"source":{"target_id":"t","virtual_id":"42","cluster":"Video","input_name":"in"},
            "program_title":"Film","visibility":"pubic"}"#;
    assert!(serde_json::from_str::<CreateRecordingRequest>(body).is_err());
}

#[test]
fn create_request_rejects_an_unknown_source_field() {
    let body = r#"{"source":{"target_id":"t","virtual_id":"42","cluster":"Video","input_name":"in",
            "stream_url":"http://evil/"},"program_title":"Film","visibility":"private"}"#;
    assert!(serde_json::from_str::<CreateRecordingRequest>(body).is_err());
}

#[test]
fn only_live_is_scheduled_and_only_media_is_resumable() {
    assert!(RecordingKind::Live.is_scheduled());
    assert!(!RecordingKind::Live.is_resumable());
    for kind in [RecordingKind::Vod, RecordingKind::Series] {
        assert!(!kind.is_scheduled(), "{kind} must not be scheduled");
        assert!(kind.is_resumable(), "{kind} must be resumable");
    }
}

#[test]
fn live_metadata_carries_padded_interval_and_source() {
    let meta = RecordingMetadata::new_live(
        test_owner(),
        RecordingVisibility::Private,
        test_source(),
        1_700_000_000,
        1_700_000_900,
        60,
        120,
    );
    assert_eq!(meta.scheduled_start, Some(1_700_000_000 - 60));
    assert_eq!(meta.scheduled_end, Some(1_700_000_900 + 120));
    assert_eq!(meta.pre_roll_secs, 60);
    assert_eq!(meta.post_roll_secs, 120);
    assert!(!meta.is_deleting());
}

#[test]
fn media_metadata_has_owner_and_source_without_live_interval() {
    let source = test_source().with_cluster(super::super::XtreamCluster::Video);
    let meta = RecordingMetadata::new_media(test_owner(), RecordingVisibility::Private, source, "Movie".to_string());

    assert_eq!(meta.program_title.as_deref(), Some("Movie"));
    assert!(meta.program_start.is_none());
    assert!(meta.program_end.is_none());
    assert!(meta.scheduled_start.is_none());
    assert!(meta.scheduled_end.is_none());
    assert_eq!(meta.pre_roll_secs, 0);
    assert_eq!(meta.post_roll_secs, 0);
}

#[test]
fn recording_owner_round_trips_as_adjacent_tag() {
    let owner = RecordingOwner::User(UserId::from("web:alice"));
    let json = serde_json::to_string(&owner).expect("serialize");
    assert_eq!(json, r#"{"kind":"user","user":"web:alice"}"#);
    let restored: RecordingOwner = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, owner);
    assert_eq!(restored.user_id(), &UserId::from("web:alice"));
}

#[test]
fn deleting_state_round_trip() {
    let mut meta =
        RecordingMetadata::new_media(test_owner(), RecordingVisibility::Private, test_source(), "M".to_string());
    assert!(!meta.is_deleting());
    meta.deleting_previous_state = Some(DeletionPreviousState::Completed);
    assert!(meta.is_deleting());
    let json = serde_json::to_string(&meta).expect("serialize");
    let restored: RecordingMetadata = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.deleting_previous_state, Some(DeletionPreviousState::Completed));
}

#[test]
fn metadata_filename_is_the_last_relative_path_component() {
    let mut meta =
        RecordingMetadata::new_media(test_owner(), RecordingVisibility::Private, test_source(), "M".to_string());
    assert_eq!(meta.filename(), None);
    meta.relative_path = Some("users/web:abc/Pilot/pilot.ts".to_string());
    assert_eq!(meta.filename(), Some("pilot.ts"));
}

#[test]
fn notification_marker_serialization() {
    let marker = NotificationMarker::new(NotificationMarkerKind::Completed, 1_700_000_000);
    let json = serde_json::to_string(&marker).expect("serialize");
    assert!(json.contains("\"completed\""), "{json}");
}

#[test]
fn queue_revision_default_is_zero() {
    assert_eq!(QueueRevision::default().0, 0);
    assert_eq!(format!("{}", QueueRevision(42)), "42");
}

// --- RecordingTaskDto ---

fn make_dto() -> RecordingTaskDto {
    RecordingTaskDto {
        id: "rec-1".to_string(),
        title: "Pilot".to_string(),
        kind: RecordingKind::Series,
        priority: TaskPriorityDto::Normal,
        status: TransferStatusDto::Running,
        retry_attempts: 1,
        transferred_bytes: 128,
        total_bytes: Some(4096),
        next_retry_at: None,
        error: None,
        restart_from_beginning_required: false,
        owner_id: Some(UserId::from("web:abc")),
        visibility: RecordingVisibility::Private,
        channel_id: None,
        channel_name: None,
        program_title: Some("Pilot".to_string()),
        program_start: None,
        program_end: None,
        scheduled_start: None,
        scheduled_end: None,
        pre_roll_secs: 0,
        post_roll_secs: 0,
        completed_at: None,
        filename: Some("pilot.mkv".to_string()),
        epg: Some(EpgEpisodeMetadata {
            programme_id: None,
            series_id: Some("series-1".to_string()),
            episode_id: Some("ep-1".to_string()),
            season: Some(1),
            episode: Some(2),
            airing: AiringStatus::New,
        }),
        rule_id: None,
        occurrence_key: None,
        allowed_actions: RecordingAllowedActions::default(),
    }
}

#[test]
fn recording_task_dto_round_trips_preserving_kind() {
    let dto = make_dto();
    let json = serde_json::to_string(&dto).expect("serialize");
    let restored: RecordingTaskDto = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, dto);
    assert_eq!(restored.kind, RecordingKind::Series);
}

#[test]
fn recording_task_dto_has_no_nested_transfer_or_recording_wrapper() {
    let json = serde_json::to_value(make_dto()).expect("serialize");
    assert!(json.get("recording").is_none(), "no nested recording wrapper");
    assert!(json.get("transfer").is_none(), "no nested transfer wrapper");
    assert_eq!(json.get("kind").and_then(serde_json::Value::as_str), Some("series"));
}

#[test]
fn terminal_partition_matches_completed_failed_cancelled() {
    for status in [TransferStatusDto::Completed, TransferStatusDto::Failed, TransferStatusDto::Cancelled] {
        assert!(RecordingTaskDto { status, ..make_dto() }.is_terminal(), "{status:?} is terminal");
    }
    for status in [
        TransferStatusDto::Scheduled,
        TransferStatusDto::Queued,
        TransferStatusDto::WaitingForCapacity,
        TransferStatusDto::RetryWaiting,
        TransferStatusDto::Running,
        TransferStatusDto::Paused,
    ] {
        assert!(!RecordingTaskDto { status, ..make_dto() }.is_terminal(), "{status:?} is current");
    }
}

#[test]
fn scheduled_duration_is_derived_from_the_padded_window() {
    let dto = RecordingTaskDto {
        kind: RecordingKind::Live,
        scheduled_start: Some(1_700_000_000),
        scheduled_end: Some(1_700_003_600),
        ..make_dto()
    };
    assert_eq!(dto.scheduled_duration_secs(), Some(3_600));
    assert_eq!(make_dto().scheduled_duration_secs(), None);
}
