use super::{config_with_hls_cache, test_app_config, HlsProxyManager};
use crate::HlsSessionKey;
use shared::model::{HlsCacheConfigDto, HlsManifestRecoveryBurstLevel, HlsStripConfigDto, HlsStripMode};
use tuliprox_core::model::HlsCacheConfig;

#[tokio::test]
async fn update_config_applies_hls_runtime_settings_to_existing_manager() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let initial_dto = HlsCacheConfigDto {
        cache_path: Some(temp_dir.path().join("hls-a").to_string_lossy().into_owned()),
        max_segments_prefetch: 1,
        ..Default::default()
    };
    let initial_config = HlsCacheConfig::from(&initial_dto);
    let manager = HlsProxyManager::with_hls_cache_config(&initial_config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 100).await;

    let mut updated_dto = initial_dto.clone();
    updated_dto.max_segments_prefetch = 4;
    updated_dto.max_concurrent_segment_fetches_per_session = 5;
    updated_dto.max_concurrent_segment_fetches_global = 6;
    updated_dto.origin_manifest_timeout_ms = shared::model::Millis::new(1_234);
    updated_dto.origin_segment_timeout_ms = shared::model::HlsSegmentTimeout::Fixed(shared::model::Millis::new(5_678));
    updated_dto.cache_duration = shared::model::Secs::new(99);
    updated_dto.session_idle_timeout = shared::model::Secs::new(55);
    updated_dto.manifest_recovery_burst.level = shared::model::HlsManifestRecoveryBurstLevel::Balanced;
    updated_dto.strip = HlsStripConfigDto { mode: HlsStripMode::Seconds, value: 7 };
    let app_config = test_app_config(config_with_hls_cache(updated_dto));

    manager.update_config(&app_config).await;

    assert_eq!(manager.segment_fetch_policy().max_prefetch_queue_depth, 4);
    assert_eq!(manager.segment_fetch_policy().max_session_segment_fetches, 5);
    assert_eq!(manager.segment_fetch_policy().max_global_segment_fetches, 6);
    assert_eq!(manager.segment_fetch_policy().segment_timeout_ms(None), 5_678);
    assert_eq!(manager.origin_manifest_timeout_ms(), 1_234);
    assert_eq!(manager.cache_duration_seconds(), 99);
    assert_eq!(manager.transient_resource_ttl_ms(), 99_000);
    assert_eq!(manager.session_idle_timeout_ms(), 55_000);
    assert_eq!(manager.manifest_recovery_burst().level, HlsManifestRecoveryBurstLevel::Balanced);
    assert_eq!(manager.strip().mode, HlsStripMode::Seconds);
    assert_eq!(manager.strip().value, 7);
    assert_eq!(session.read().await.segment_prefetch_queue.max_prefetch_depth(), 4);
    assert_eq!(session.read().await.live_window_strip, manager.strip());
}

#[tokio::test]
async fn optional_hls_config_controls_runtime_enabled_state() {
    let manager = HlsProxyManager::from_hls_cache_config(None);

    assert!(!manager.is_enabled());
    assert_eq!(
        manager.run_garbage_collection_once(1_000).await.expect("disabled gc should no-op"),
        super::super::super::GarbageCollectionReport::default()
    );

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let enabled_dto =
        HlsCacheConfigDto { cache_path: Some(temp_dir.path().to_string_lossy().into_owned()), ..Default::default() };
    let app_config = test_app_config(config_with_hls_cache(enabled_dto));
    manager.update_config(&app_config).await;

    assert!(manager.is_enabled());
}

#[tokio::test]
async fn update_config_cache_path_change_clears_runtime_cache_state() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let old_cache = temp_dir.path().join("old");
    let new_cache = temp_dir.path().join("new");
    let initial_dto =
        HlsCacheConfigDto { cache_path: Some(old_cache.to_string_lossy().to_string()), ..Default::default() };
    let initial_config = HlsCacheConfig::from(&initial_dto);
    let manager = HlsProxyManager::with_hls_cache_config(&initial_config);
    let _ = manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 100).await;
    assert_eq!(manager.sessions().len().await, 1);

    let mut updated_dto = initial_dto;
    updated_dto.cache_path = Some(new_cache.to_string_lossy().to_string());
    let app_config = test_app_config(config_with_hls_cache(updated_dto));

    manager.update_config(&app_config).await;

    assert!(manager.sessions().is_empty().await);
    assert_eq!(manager.segment_cache().cache_path(), new_cache);
}

#[test]
fn targeted_timeout_update_reaches_existing_worker_policies_without_replacing_slots() {
    use crate::HlsResourceFetchKind::{Map, Segment};
    use shared::model::{HlsSegmentTimeout, Millis, Secs};
    let manager = HlsProxyManager::new();
    let segment_policy = manager.segment_worker_pool().policy();
    let map_policy = manager.map_worker_pool().policy();
    let frozen = segment_policy.timeouts.snapshot(Segment, Some(2));
    manager.update_origin_preferences(Secs::new(0), HlsSegmentTimeout::Fixed(Millis::new(700)), 800);
    assert_eq!(frozen.effective_ms, 2_000);
    assert_eq!(segment_policy.timeouts.snapshot(Segment, Some(12)).effective_ms, 700);
    assert_eq!(map_policy.timeouts.snapshot(Map, None).effective_ms, 800);
    assert_eq!(manager.origin_manifest_timeout_ms(), 800);
}

#[tokio::test]
async fn initial_and_reloaded_connect_budgets_use_the_loaded_config() {
    use shared::model::{HlsSegmentTimeout, Millis};
    let directory = tempfile::tempdir().unwrap();
    let hls = HlsCacheConfigDto {
        cache_path: Some(directory.path().to_string_lossy().into_owned()),
        origin_segment_timeout_ms: HlsSegmentTimeout::Fixed(Millis::new(2_000)),
        origin_manifest_timeout_ms: Millis::new(8_000),
        ..Default::default()
    };
    for connect_secs in [0, 30, u32::MAX] {
        let mut config = config_with_hls_cache(hls.clone());
        config.connect_timeout_secs = connect_secs;
        let manager = HlsProxyManager::from_config(&config, b"secret");
        let connect_ms = u64::from(connect_secs) * 1_000;
        // Five attempts for each of Key and Segment, including their separate
        // provider/transfer/connect phases, plus existing backoff and margins.
        let expected_wait_ms = 5 * (20_000 + 2 * connect_ms) + 5_200;
        for policy in
            [manager.segment_fetch_policy(), manager.segment_worker_pool().policy(), manager.map_worker_pool().policy()]
        {
            assert_eq!(policy.timeouts.config().connect_ms, connect_ms);
            assert_eq!(policy.demand_wait_timeout().as_millis(), u128::from(expected_wait_ms));
        }
        let recovery = crate::hls_recovery_timing_policy(&manager, 8_000, Some(12));
        assert_eq!(recovery.media_operation_timeout.as_millis(), expected_wait_ms);

        let existing_worker_policy = manager.segment_worker_pool().policy();
        config.connect_timeout_secs = 45;
        manager.update_config(&test_app_config(config)).await;
        assert_eq!(existing_worker_policy.timeouts.config().connect_ms, 45_000);
        assert_eq!(manager.map_worker_pool().policy().timeouts.config().connect_ms, 45_000);
    }
}

#[test]
fn managers_built_with_defaults_never_share_a_cache_directory() {
    // Managers created in parallel tests used to share `DEFAULT_HLS_CACHE_PATH`; the orphan-session
    // GC of one then deleted the segment files of another.
    let first = HlsProxyManager::new();
    let second = HlsProxyManager::new();
    let first_path = first.segment_cache().cache_path();
    let second_path = second.segment_cache().cache_path();

    assert_ne!(first_path, second_path);
    assert_ne!(first_path, std::path::PathBuf::from(crate::cache::DEFAULT_HLS_CACHE_PATH));
    assert_ne!(second_path, std::path::PathBuf::from(crate::cache::DEFAULT_HLS_CACHE_PATH));
}

#[tokio::test]
async fn private_cache_helpers_never_resolve_to_the_shared_default_path() {
    let shared_default = std::path::PathBuf::from(tuliprox_core::model::default_hls_cache_path());

    let manager = HlsProxyManager::with_private_cache(&HlsCacheConfigDto::default());
    let private_path = manager.segment_cache().cache_path();
    assert_ne!(private_path, shared_default);
    assert_ne!(
        private_path,
        HlsProxyManager::with_private_cache(&HlsCacheConfigDto::default()).segment_cache().cache_path()
    );

    let kept = manager.keep_cache_path(HlsCacheConfigDto::default());
    assert_eq!(kept.cache_path.as_deref().map(std::path::Path::new), Some(private_path.as_path()));
    manager.update_config(&test_app_config(config_with_hls_cache(kept))).await;
    assert_eq!(manager.segment_cache().cache_path(), private_path);

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let explicit =
        HlsCacheConfigDto { cache_path: Some(temp_dir.path().to_string_lossy().into_owned()), ..Default::default() };
    assert_eq!(manager.keep_cache_path(explicit.clone()).cache_path, explicit.cache_path);
    assert_eq!(HlsProxyManager::with_private_cache(&explicit).segment_cache().cache_path(), temp_dir.path());
}

#[tokio::test]
async fn default_manager_removes_its_private_cache_directory_on_drop() {
    let manager = HlsProxyManager::new();
    let cache_path = manager.segment_cache().cache_path();
    tokio::fs::create_dir_all(cache_path.join("session")).await.expect("create session dir");

    drop(manager);

    assert!(!cache_path.exists());
}

#[test]
fn private_cache_dir_is_created_and_removed_by_its_owner() {
    let dir = super::super::config::PrivateCacheDir::create().expect("create private cache dir");
    let path = dir.0.clone();
    assert!(path.is_dir());

    drop(dir);

    assert!(!path.exists());
}

#[test]
fn private_cache_dir_never_adopts_an_existing_directory() {
    let first = super::super::config::PrivateCacheDir::create().expect("create private cache dir");
    let first_path = first.0.clone();
    std::fs::write(first_path.join("stale"), b"stale").expect("write stale file");

    let second = super::super::config::PrivateCacheDir::create().expect("create private cache dir");

    assert_ne!(second.0, first_path);
    assert!(first_path.join("stale").exists(), "creating another directory must not touch an existing one");
    assert!(std::fs::read_dir(&second.0).expect("read new dir").next().is_none());
}
