use super::{
    create_test_worker, load_metadata_retry_states_from_disk, persist_metadata_retry_state_to_disk, InputWorker,
    MetadataUpdateRuntimeSettings, RetryDomain, RetryState, TaskKey, TaskRetryState,
};
use dashmap::DashMap;
use std::sync::{atomic::AtomicUsize, Arc};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tuliprox_core::model::{ProviderIdType, ResolveReason, UpdateTask};
use tuliprox_repository::RetryStateDbValue;

#[tokio::test]
async fn flush_dirty_retry_states_retries_persist_after_transient_failure() {
    let dir = tempdir().expect("tempdir should be created");
    let failing_path = dir.path().join("missing").join("metadata_retry_state.db");
    let success_path = dir.path().join("metadata_retry_state.db");
    let key = TaskKey::Vod(42);
    let state = TaskRetryState {
        resolve: Some(RetryState {
            attempts: 2,
            next_allowed_at_ts: 1_700_043_200,
            cooldown_until_ts: None,
            last_error: Some("temporary failure".to_string()),
            source_last_modified: None,
        }),
        probe: None,
        tmdb: None,
        updated_at_ts: 1_700_000_000,
    };
    let (tx, rx) = mpsc::channel::<TaskKey>(8);
    let pending_tasks = Arc::new(DashMap::new());
    let pending_task_count = Arc::new(AtomicUsize::new(0));
    let mut worker = create_test_worker("input_retry_flush", tx, rx, pending_tasks, pending_task_count);

    worker.metadata_retry_state_path = Some(failing_path);
    worker.retry_states.insert(key.clone(), state.clone());
    worker.dirty_retry_state_keys.insert(key.clone());

    worker.persist_metadata_retry_state(&key, Some(&state)).await;
    assert!(worker.dirty_retry_state_keys.contains(&key));

    worker.metadata_retry_state_path = Some(success_path.clone());
    worker.flush_dirty_retry_states().await;

    assert!(!worker.dirty_retry_state_keys.contains(&key));
    let loaded = load_metadata_retry_states_from_disk(&success_path).expect("state load should succeed after retry");
    let loaded_state = loaded.get(&key).expect("retry state should be persisted on flush");
    let loaded_resolve = loaded_state.resolve.as_ref().expect("resolve retry state should be present");
    assert_eq!(loaded_resolve.attempts, 2);
    assert_eq!(loaded_resolve.last_error.as_deref(), Some("temporary failure"));
}

#[test]
fn metadata_retry_state_disk_roundtrip() {
    let dir = tempdir().expect("tempdir should be created");
    let path = dir.path().join("metadata_retry_state.db");
    let key = TaskKey::Stream { scope: Arc::from("input_a"), id: Arc::from("stream_1") };
    let state = TaskRetryState {
        resolve: Some(RetryState {
            attempts: 4,
            next_allowed_at_ts: 1_700_043_200,
            cooldown_until_ts: Some(1_700_043_200),
            last_error: Some("resolve exhausted".to_string()),
            source_last_modified: None,
        }),
        probe: Some(RetryState {
            attempts: 3,
            next_allowed_at_ts: 1_700_000_000,
            cooldown_until_ts: Some(1_700_086_400),
            last_error: Some("probe timeout".to_string()),
            source_last_modified: None,
        }),
        tmdb: Some(RetryState {
            attempts: 0,
            next_allowed_at_ts: 1_700_172_800,
            cooldown_until_ts: Some(1_700_172_800),
            last_error: Some("tmdb no match".to_string()),
            source_last_modified: Some(999),
        }),
        updated_at_ts: 1_700_000_000,
    };

    persist_metadata_retry_state_to_disk(&path, &key, Some(&state)).expect("state persistence should succeed");
    let loaded = load_metadata_retry_states_from_disk(&path).expect("state load should succeed");
    let loaded_state = loaded.get(&key).expect("probe key should be present");

    let loaded_resolve = loaded_state.resolve.as_ref().expect("resolve retry state should be present");
    assert_eq!(loaded_resolve.attempts, 4);
    assert_eq!(loaded_resolve.cooldown_until_ts, Some(1_700_043_200));
    assert_eq!(loaded_resolve.last_error.as_deref(), Some("resolve exhausted"));
    let loaded_probe = loaded_state.probe.as_ref().expect("probe retry state should be present");
    assert_eq!(loaded_probe.attempts, 3);
    assert_eq!(loaded_probe.cooldown_until_ts, Some(1_700_086_400));
    assert_eq!(loaded_probe.last_error.as_deref(), Some("probe timeout"));
    let loaded_tmdb = loaded_state.tmdb.as_ref().expect("tmdb retry state should be present");
    assert_eq!(loaded_tmdb.cooldown_until_ts, Some(1_700_172_800));
    assert_eq!(loaded_tmdb.last_error.as_deref(), Some("tmdb no match"));
    assert_eq!(loaded_tmdb.source_last_modified, Some(999));

    persist_metadata_retry_state_to_disk(&path, &key, None).expect("state clear should succeed");
    let cleared = load_metadata_retry_states_from_disk(&path).expect("state reload should succeed");
    assert!(!cleared.contains_key(&key));
}

#[test]
fn msgpack_backward_compat_old_4field_retry_state() {
    // Simulate old 4-field format (before source_last_modified was added)
    #[derive(Debug, serde::Serialize)]
    struct OldRetryStateDbValue {
        attempts: u8,
        next_allowed_at_ts: i64,
        cooldown_until_ts: Option<i64>,
        last_error: Option<String>,
    }

    let old = OldRetryStateDbValue {
        attempts: 3,
        next_allowed_at_ts: 1_700_000_000,
        cooldown_until_ts: Some(1_700_043_200),
        last_error: Some("HTTP 404".to_string()),
    };

    let bytes = rmp_serde::to_vec(&old).expect("old format should serialize");
    let result = rmp_serde::from_slice::<RetryStateDbValue>(&bytes);
    assert!(result.is_ok(), "deserializing old 4-field format into new 5-field struct should succeed, got: {result:?}");
    let loaded = result.unwrap();
    assert_eq!(loaded.attempts, 3);
    assert_eq!(loaded.cooldown_until_ts, Some(1_700_043_200));
    assert_eq!(loaded.source_last_modified, None);
}

#[test]
fn probe_backoff_steps_follow_expected_windows() {
    let runtime_settings = MetadataUpdateRuntimeSettings::default();
    let first = InputWorker::compute_probe_retry_backoff_secs(1, &runtime_settings);
    let second = InputWorker::compute_probe_retry_backoff_secs(2, &runtime_settings);
    let third = InputWorker::compute_probe_retry_backoff_secs(3, &runtime_settings);

    assert!((480..=720).contains(&first), "expected ~10m with jitter, got {first}");
    assert!((1_440..=2_160).contains(&second), "expected ~30m with jitter, got {second}");
    assert!((2_880..=4_320).contains(&third), "expected ~60m with jitter, got {third}");
}

#[test]
fn retry_domain_uses_probe_for_probe_only_resolve_tasks() {
    let task = UpdateTask::ResolveVod {
        id: ProviderIdType::Id(7),
        reason: ResolveReason::Probe.into(),
        delay: 0,
        source_last_modified: None,
    };
    assert_eq!(InputWorker::retry_domain_for_task(&task), RetryDomain::Probe);
}

#[test]
fn retry_domain_keeps_resolve_for_mixed_resolve_tasks() {
    let task = UpdateTask::ResolveSeries {
        id: ProviderIdType::Id(11),
        reason: ResolveReason::Probe | ResolveReason::Info,
        delay: 0,
        source_last_modified: None,
    };
    assert_eq!(InputWorker::retry_domain_for_task(&task), RetryDomain::Resolve);
}
