use super::{
    control::cancel_future_rule_recordings_in_candidate, error::map_edit_validation_error,
    filenames::render_live_filename, identity::candidate_has_duplicate_recording, window::effective_recording_window,
    *,
};
use crate::{
    recording::recording_queue::{mutate, PersistedRecordingQueue, RecordingTask, RecordingTaskState},
    recording_edit::EditError,
};
use shared::model::{
    recording::{RecordingMetadata, RecordingOwner, RecordingProvenance, RecordingSource, RecordingVisibility},
    Permission, RecordingContainerFormat, RecordingKind, UserId, XtreamCluster,
};
use std::collections::HashMap;
use tuliprox_core::model::{RecordingConfig, RecordingNotificationConfig};

fn source(target_name: &str, virtual_id: &str, input_name: &str) -> RecordingSourceInput {
    RecordingSourceInput {
        target_id: target_name.to_string(),
        virtual_id: virtual_id.to_string(),
        cluster: XtreamCluster::Live,
        input_name: input_name.to_string(),
    }
}

fn create_input() -> CreateRecordingInput {
    CreateRecordingInput {
        source: source("target", "1", "input-a"),
        program_title: "Pilot".to_string(),
        program_start: 1_700_000_000,
        program_end: 1_700_003_600,
        pre_roll_secs: 0,
        post_roll_secs: 0,
        visibility: RecordingVisibility::Private,
        channel_id: None,
        channel_name: None,
        group: None,
        provenance: RecordingProvenance::default(),
        epg: None,
    }
}

fn persisted_media(uuid: &str, owner: &str, visibility: RecordingVisibility, url: &str) -> PersistedRecordingTask {
    let meta = RecordingMetadata::new_media(
        RecordingOwner::User(UserId::from(owner)),
        visibility,
        RecordingSource::new("1", "1", "input-a"),
        "Film".to_string(),
    );
    PersistedRecordingTask {
        media_identity: String::new(),
        partition: crate::recording::recording_queue::RecordingPartition::default(),
        uuid: uuid.to_string(),
        kind: RecordingKind::Vod,
        file_dir: std::path::PathBuf::from("/tmp"),
        file_path: std::path::PathBuf::from(format!("/tmp/{uuid}.mp4")),
        filename: format!("{uuid}.mp4"),
        url: url.to_string(),
        finished: false,
        size: 0,
        total_size: None,
        paused: false,
        error: None,
        state: RecordingTaskState::Queued,
        input_name: Some("input-a".to_string()),
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: meta,
    }
}

fn queued_candidate(tasks: Vec<PersistedRecordingTask>) -> PersistedRecordingQueue {
    PersistedRecordingQueue { queue: tasks, ..PersistedRecordingQueue::default() }
}

fn persisted_rule_recording(uuid: &str, rule_id: Option<&str>, start_at: i64) -> PersistedRecordingTask {
    let mut meta = RecordingMetadata::new_live(
        RecordingOwner::User(UserId::from("web:alice")),
        RecordingVisibility::Private,
        RecordingSource::new("1", "1", "input-a"),
        start_at,
        start_at + 3_600,
        0,
        0,
    );
    meta.reserved_bytes = 123;
    meta.provenance.rule_id = rule_id.map(str::to_string);
    PersistedRecordingTask {
        media_identity: String::new(),
        partition: crate::recording::recording_queue::RecordingPartition::default(),
        uuid: uuid.to_string(),
        file_dir: std::path::PathBuf::from("/tmp"),
        file_path: std::path::PathBuf::from(format!("/tmp/{uuid}.ts")),
        filename: format!("{uuid}.ts"),
        url: "http://example.test/live.ts".to_string(),
        finished: false,
        size: 0,
        total_size: None,
        paused: false,
        error: None,
        state: RecordingTaskState::Scheduled,
        kind: RecordingKind::Live,
        input_name: Some("input-a".to_string()),
        priority: 0,
        retry_attempts: 0,
        next_retry_at: None,
        recording: meta,
    }
}

fn deleting_claims() -> shared::model::Claims {
    shared::model::Claims {
        username: "alice".to_string(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: 0,
        roles: shared::model::RoleSet::new(),
        permissions: Permission::RecordingDelete.into(),
        pwd_version: 0,
        subject_id: Some(UserId::from("web:alice")),
        permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
    }
}

/// A finished entry `uuid` of `owner` on the shared film, with its file in
/// `dir`. Both the final file and the partial are written, so a test sees
/// exactly which one a removal touched.
async fn finished_film(
    queue: &RecordingQueue,
    dir: &std::path::Path,
    uuid: &str,
    owner: &str,
    state: RecordingTaskState,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let mut task = persisted_media(uuid, owner, RecordingVisibility::Private, "http://provider/film.mp4");
    task.file_dir = dir.to_path_buf();
    task.file_path = dir.join("recording.mp4");
    task.state = state;
    task.finished = true;
    let final_path = task.file_path.clone();
    let partial = crate::recording::recording_worker::recording_partial_path(&final_path);
    std::fs::write(&final_path, b"recorded bytes").expect("write final file");
    std::fs::write(&partial, b"partial bytes").expect("write partial");
    let task = RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task).expect("valid task"));
    mutate(queue, move |candidate| {
        candidate.finished.push(task.clone());
        Ok(())
    })
    .await
    .expect("seed recording");
    (final_path, partial)
}

fn editing_claims() -> shared::model::Claims {
    shared::model::Claims {
        username: "alice".to_string(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: 0,
        roles: shared::model::RoleSet::new(),
        permissions: Permission::RecordingManage.into(),
        pwd_version: 0,
        subject_id: Some(UserId::from("web:alice")),
        permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
    }
}

/// A scheduled live recording an hour from now, for `owner`.
fn upcoming_live(uuid: &str, owner: &str, start: i64) -> PersistedRecordingTask {
    let mut task = persisted_rule_recording(uuid, None, start);
    task.recording.owner = RecordingOwner::User(UserId::from(owner));
    task.file_path = std::path::PathBuf::from("/tmp/shared-programme.ts");
    task.filename = "shared-programme.ts".to_string();
    task.recording.relative_path = Some("shared-programme.ts".to_string());
    RecordingQueue::to_persisted(&RecordingQueue::from_persisted(task).expect("valid task"))
}

async fn scheduled_queue(tasks: Vec<PersistedRecordingTask>) -> Arc<RecordingQueue> {
    let queue = Arc::new(RecordingQueue::new());
    mutate(&queue, move |candidate| {
        candidate.scheduled.clone_from(&tasks);
        Ok(())
    })
    .await
    .expect("seed");
    queue
}

/// The record set the repository actually holds, for tests that
/// assert a rejected mutation left persistence untouched.
async fn committed_records(queue: &RecordingQueue) -> Vec<PersistedRecordingTask> {
    let repository = queue.repository.clone().expect("queue is repository backed");
    tokio::task::spawn_blocking(move || {
        let mut guard = repository.lock().expect("repository lock");
        guard.load()
    })
    .await
    .expect("join")
    .expect("load")
    .tasks
}

fn test_app_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::Config::default())),
        sources: Arc::new(arc_swap::ArcSwap::from_pointee(tuliprox_core::model::SourcesConfig::default())),
        hdhomerun: Arc::new(arc_swap::ArcSwapOption::empty()),
        api_proxy: Arc::new(arc_swap::ArcSwapOption::empty()),
        file_locks: Arc::new(tuliprox_core::utils::FileLockManager::default()),
        paths: Arc::new(arc_swap::ArcSwap::from_pointee(shared::model::ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(arc_swap::ArcSwapOption::empty()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(tuliprox_core::model::MediaToolCapabilities::default()),
    })
}

fn filename_config(template: &str, container_format: RecordingContainerFormat) -> RecordingConfig {
    let mut cfg = RecordingConfig::from(&shared::model::RecordingConfigDto::default());
    cfg.filename_template = template.to_string();
    cfg.container_format = container_format;
    cfg.timezone = "Europe/Berlin".parse().expect("timezone parses");
    cfg
}

fn filename_window(input: &CreateRecordingInput) -> EffectiveRecordingWindow {
    EffectiveRecordingWindow {
        scheduled_start: input.program_start,
        scheduled_end: input.program_end,
        execution_start: input.program_start,
        remaining_duration_secs: 0,
    }
}

fn filename_input() -> CreateRecordingInput {
    let mut input = create_input();
    input.program_title = "Hart van Nederland".to_string();
    input.channel_name = Some("┃NLZIET┃ SBS 6 HD".to_string());
    // 2023-11-14 22:13:20 UTC = 23:13 in Berlin.
    input.program_start = 1_700_000_000;
    input.program_end = 1_700_001_800;
    input
}

/// A service rooted at `dir` with the supplied disk block, and the
/// source and server configuration a recording needs to resolve its
/// own target and URL.
fn service_with_disk(
    dir: &std::path::Path,
    queue: &Arc<RecordingQueue>,
    disk: Option<tuliprox_core::model::RecordingDiskConfig>,
) -> RecordingService {
    let mut rec_cfg = RecordingConfig::from(&shared::model::RecordingConfigDto { enabled: true, ..Default::default() });
    rec_cfg.directory = dir.to_string_lossy().into_owned();
    rec_cfg.disk = disk;
    let config = tuliprox_core::model::Config {
        video: Some(tuliprox_core::model::VideoConfig {
            extensions: Vec::new(),
            web_search: None,
            recording: Some(rec_cfg),
        }),
        ..tuliprox_core::model::Config::default()
    };

    let input = Arc::new(tuliprox_core::model::ConfigInput { id: 7, name: "input-a".into(), ..Default::default() });
    let target = Arc::new(tuliprox_core::model::ConfigTarget {
        id: 11,
        enabled: true,
        name: "1".to_string(),
        options: None,
        sort: None,
        filter: tuliprox_core::model::StagedFilter::default(),
        output: vec![],
        rename: None,
        mapping_ids: None,
        mapping: Arc::default(),
        favourites: None,
        processing_order: shared::model::ProcessingOrder::default(),
        curation: None,
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    });
    let sources = tuliprox_core::model::SourcesConfig {
        inputs: vec![Arc::clone(&input)],
        sources: vec![tuliprox_core::model::ConfigSource { inputs: vec!["input-a".into()], targets: vec![target] }],
        ..tuliprox_core::model::SourcesConfig::default()
    };

    let app_config = test_app_config();
    app_config.config.store(Arc::new(config));
    app_config.sources.store(Arc::new(sources));
    RecordingService::new(Arc::clone(queue), app_config)
}

fn creating_claims() -> shared::model::Claims {
    shared::model::Claims {
        username: "alice".to_string(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: 0,
        roles: shared::model::RoleSet::new(),
        permissions: Permission::RecordingCreate | Permission::RecordingManage | Permission::RecordingDelete,
        pwd_version: 0,
        subject_id: Some(UserId::from("web:alice")),
        permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
    }
}

fn claims_for(user: &str, permissions: shared::model::permission::PermissionSet) -> shared::model::Claims {
    shared::model::Claims {
        username: user.to_string(),
        subject_id: Some(UserId::from(format!("web:{user}"))),
        permissions,
        ..creating_claims()
    }
}

fn media_input() -> CreateMediaRecordingInput {
    CreateMediaRecordingInput {
        source: RecordingSourceInput {
            target_id: "1".to_string(),
            virtual_id: "77".to_string(),
            cluster: XtreamCluster::Video,
            input_name: "input-a".to_string(),
        },
        title: "The Film".to_string(),
        extension: "mp4".to_string(),
        visibility: RecordingVisibility::Private,
        group: None,
        series_name: None,
    }
}

fn with_private_quota(service: &RecordingService, bytes: u64) {
    let mut config = tuliprox_core::model::Config::clone(&service.app_config.config.load());
    if let Some(recording) = config.video.as_mut().and_then(|video| video.recording.as_mut()) {
        recording.quota = Some(tuliprox_core::model::RecordingQuotaConfig {
            default_private_bytes: Some(bytes),
            ..Default::default()
        });
    }
    service.app_config.config.store(Arc::new(config));
}

/// Alice's request for the film, finished at `bytes`.
async fn completed_film(service: &RecordingService, queue: &RecordingQueue, bytes: u64) {
    service.create_media_recording_idempotent(&creating_claims(), &media_input(), None).await.expect("alice's request");
    mutate(queue, |candidate| {
        let mut done = candidate.queue.remove(0);
        done.state = RecordingTaskState::Completed;
        done.finished = true;
        done.size = bytes;
        done.total_size = Some(bytes);
        done.recording.measured_bytes = bytes;
        candidate.finished.push(done);
        Ok(())
    })
    .await
    .expect("complete");
}

fn disk_test_input() -> CreateRecordingInput {
    let now = chrono::Utc::now().timestamp();
    CreateRecordingInput {
        source: RecordingSourceInput {
            target_id: "1".to_string(),
            virtual_id: "42".to_string(),
            cluster: XtreamCluster::Live,
            input_name: "input-a".to_string(),
        },
        program_title: "title".to_string(),
        program_start: now,
        program_end: now + 600,
        pre_roll_secs: 0,
        post_roll_secs: 0,
        visibility: RecordingVisibility::Private,
        channel_id: None,
        channel_name: None,
        group: None,
        provenance: RecordingProvenance::default(),
        epg: None,
    }
}

mod admission;
mod http;
mod lifecycle;
mod playlist;
mod policy;
mod storage;
mod transport;
