use super::{test_app_config, test_app_state};
use crate::model::{Config, ConfigInput, ConfigSource};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use shared::{
    model::{
        InputRefreshPolicy, InputType, PersistedPlaylistUpdateClusterSnapshot, PersistedPlaylistUpdateClusterState,
        PlaylistUpdateDataSource, PlaylistUpdateStatusDto, XtreamCluster,
    },
    utils::Internable,
};
use std::sync::Arc;
use tempfile::tempdir;
use tower::ServiceExt;

#[tokio::test]
async fn playlist_update_status_reloads_staged_own_completion_without_default_or_parent_inference() {
    use crate::processing::input_cache::{
        load_input_status, resolve_input_storage_path, save_input_status, ClusterState, ClusterStatus, InputStatus,
    };
    use shared::model::{
        PersistedPlaylistUpdateClusterStatusDto, PersistedPlaylistUpdateInputResult, PlaylistUpdateState,
    };
    for own_state in [Some(PlaylistUpdateState::Success), Some(PlaylistUpdateState::Failure), None] {
        let temp = tempdir().unwrap();
        let parent = Arc::new(ConfigInput {
            id: 7,
            name: "parent".intern(),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        });
        let app_config =
            test_app_config(parent.clone(), ConfigSource { inputs: vec![parent.name.clone()], targets: Vec::new() });
        app_config
            .config
            .store(Arc::new(Config { storage_dir: temp.path().to_string_lossy().into_owned(), ..Config::default() }));
        let mut staged = ConfigInput {
            id: 8,
            name: "staged/provider own".intern(),
            input_type: InputType::Staged,
            staged_type: shared::model::StagedInputType::Xtream,
            ..ConfigInput::default()
        };
        staged.staged = Some(tuliprox_core::model::ConfigInputStaged {
            for_input: Some(parent.name.clone()),
            clusters: shared::model::ClusterFlags::Live,
        });
        staged.resolve_staged_download_type();
        let mut sources = app_config.sources.load().as_ref().clone();
        sources.inputs.insert(0, Arc::new(staged.clone())); // Deliberately not ID or source order.
        app_config.sources.store(Arc::new(sources));
        let path = resolve_input_storage_path(&temp.path().to_string_lossy(), &staged.name).await;
        let snapshot = PersistedPlaylistUpdateClusterSnapshot {
            policy: Some(InputRefreshPolicy::REFRESH),
            source: Some(PlaylistUpdateDataSource::Cache),
            quality_guard_threshold: Some(95),
            ..PersistedPlaylistUpdateClusterSnapshot::default()
        };
        let mut persisted = InputStatus::default();
        persisted.clusters.insert(
            "live".into(),
            ClusterStatus { status: ClusterState::Ok, timestamp: 123, last_update: Some(snapshot) },
        );
        persisted.last_input_update =
            own_state.map(|state| PersistedPlaylistUpdateInputResult { state, timestamp: 456 });
        save_input_status(&path, &persisted);
        let before = std::fs::read(path.join("status.json")).unwrap();
        let parent_path = resolve_input_storage_path(&temp.path().to_string_lossy(), &parent.name).await;
        save_input_status(
            &parent_path,
            &InputStatus {
                last_input_update: Some(PersistedPlaylistUpdateInputResult {
                    state: PlaylistUpdateState::Partial,
                    timestamp: 789,
                }),
                ..InputStatus::default()
            },
        );
        let router = super::super::v1_api_playlist_register_protected(Router::new())
            .with_state(test_app_state(Arc::new(app_config)));
        let response = router
            .into_service::<Body>()
            .oneshot(Request::builder().uri("/playlist/update/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let dto: PlaylistUpdateStatusDto = serde_json::from_slice(&body).unwrap();
        let own = dto.inputs.iter().find(|input| input.input_id == 8).unwrap();
        assert_eq!(own.last_input_update, persisted.last_input_update);
        assert_eq!(own.last_update, Some(123));
        assert_eq!(
            own.clusters,
            vec![PersistedPlaylistUpdateClusterStatusDto {
                cluster: XtreamCluster::Live,
                status: PersistedPlaylistUpdateClusterState::Ok,
                timestamp: 123,
                last_update: Some(snapshot)
            }]
        );
        assert_eq!(
            dto.inputs.iter().find(|input| input.input_id == 7).unwrap().last_input_update.unwrap().state,
            PlaylistUpdateState::Partial
        );
        assert!(!load_input_status(&path).clusters.contains_key("default"));
        assert_eq!(
            std::fs::read(path.join("status.json")).unwrap(),
            before,
            "read endpoint must not rewrite canonical status"
        );
    }
}

#[tokio::test]
async fn playlist_update_status_exposes_last_input_results_and_legacy_default_without_inventing_clusters() {
    use crate::processing::input_cache::{
        load_input_status, resolve_input_storage_path, save_input_status, ClusterState, ClusterStatus, InputStatus,
    };
    use shared::model::{PersistedPlaylistUpdateInputResult, PlaylistUpdateState};
    for last_state in [
        None,
        Some(PlaylistUpdateState::Success),
        Some(PlaylistUpdateState::Partial),
        Some(PlaylistUpdateState::Failure),
    ] {
        let temp = tempdir().unwrap();
        let app_config =
            test_app_config(Arc::new(ConfigInput::default()), ConfigSource { inputs: Vec::new(), targets: Vec::new() });
        app_config
            .config
            .store(Arc::new(Config { storage_dir: temp.path().to_string_lossy().into_owned(), ..Config::default() }));
        let mut sources = (**app_config.sources.load()).clone();
        sources.inputs.clear();
        for (index, input_type) in [
            InputType::Xtream,
            InputType::XtreamBatch,
            InputType::Stalker,
            InputType::StalkerBatch,
            InputType::M3u,
            InputType::M3uBatch,
            InputType::Library,
            InputType::Plex,
            InputType::Emby,
            InputType::Jellyfin,
            InputType::Staged,
        ]
        .into_iter()
        .enumerate()
        {
            let input = Arc::new(ConfigInput {
                id: u16::try_from(index + 1).unwrap(),
                name: format!("input-{index}").intern(),
                input_type,
                enabled: index % 2 == 0,
                ..ConfigInput::default()
            });
            let path = resolve_input_storage_path(&temp.path().to_string_lossy(), &input.name).await;
            let mut persisted = InputStatus::default();
            persisted.clusters.insert(
                "default".to_string(),
                ClusterStatus {
                    status: if input.enabled { ClusterState::Ok } else { ClusterState::Failed },
                    timestamp: 123,
                    last_update: None,
                },
            );
            persisted.last_input_update =
                last_state.map(|state| PersistedPlaylistUpdateInputResult { state, timestamp: 456 });
            save_input_status(&path, &persisted);
            sources.inputs.push(input);
        }
        let inputs = sources.inputs.clone();
        app_config.sources.store(Arc::new(sources));
        let router = super::super::v1_api_playlist_register_protected(Router::new())
            .with_state(test_app_state(Arc::new(app_config)));
        let response = router
            .into_service::<Body>()
            .oneshot(Request::builder().uri("/playlist/update/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let dto: PlaylistUpdateStatusDto = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto.inputs.len(), inputs.len());
        for (input, status) in inputs.iter().zip(dto.inputs) {
            assert_eq!(status.input_id, input.id);
            assert_eq!(status.last_update, Some(123), "Last update retains the cache timestamp contract");
            let expected = if let Some(state) = last_state {
                Some(PersistedPlaylistUpdateInputResult { state, timestamp: 456 })
            } else if input.input_type.is_xtream() || input.input_type.is_stalker() {
                None // Do not reconstruct an aggregate input result from incomplete legacy clusters.
            } else {
                Some(PersistedPlaylistUpdateInputResult {
                    state: if input.enabled { PlaylistUpdateState::Success } else { PlaylistUpdateState::Failure },
                    timestamp: 123,
                })
            };
            assert_eq!(status.last_input_update, expected);
            assert!(status.clusters.is_empty(), "no synthetic Live/VOD/Series from default");
            let path = resolve_input_storage_path(&temp.path().to_string_lossy(), &input.name).await;
            assert_eq!(
                load_input_status(&path).last_input_update.map(|result| result.state),
                last_state,
                "read endpoint must not persist a legacy projection"
            );
        }
    }
}
