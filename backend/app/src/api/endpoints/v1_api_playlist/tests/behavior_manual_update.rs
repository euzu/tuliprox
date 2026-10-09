use super::{playlist_update_target, test_app_config, test_app_state, test_app_state_with_manual_update_sender};
use crate::model::{Config, ConfigInput, ConfigSource, SourcesConfig};
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::IntoResponse,
    Json, Router,
};
use serde_json::json;
use shared::{
    model::{
        InputRefreshOverride, InputRefreshPolicy, InputType, OperationRunAccepted,
        PersistedPlaylistUpdateClusterSnapshot, PersistedPlaylistUpdateClusterState,
        PersistedPlaylistUpdateQualityDecision, PersistedPlaylistUpdateQualitySnapshot,
        PersistedPlaylistUpdateTechnicalState, PlaylistUpdateDataSource, PlaylistUpdateRequestDto,
        PlaylistUpdateRequestPayload, PlaylistUpdateStatusDto, XtreamCluster,
    },
    utils::Internable,
};
use std::{collections::HashMap, sync::Arc};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tower::ServiceExt;

#[test]
fn manual_update_capabilities_reject_unsupported_actions_and_keep_all_required_inputs() {
    use shared::model::{InputUpdateAction, InputUpdateRequest};
    for (input_type, allowed) in [
        (InputType::Xtream, 3),
        (InputType::Stalker, 3),
        (InputType::M3u, 3),
        (InputType::Plex, 2),
        (InputType::Library, 1),
        (InputType::Jellyfin, 0),
        (InputType::Emby, 0),
        (InputType::M3uBatch, 0),
        (InputType::XtreamBatch, 0),
        (InputType::StalkerBatch, 0),
        (InputType::Staged, 0),
    ] {
        for enabled in [false, true] {
            let input = Arc::new(ConfigInput {
                id: 2,
                name: "selected".intern(),
                input_type,
                enabled,
                ..ConfigInput::default()
            });
            let companion = Arc::new(ConfigInput {
                id: 3,
                name: "companion".intern(),
                input_type: InputType::M3u,
                enabled: true,
                ..ConfigInput::default()
            });
            let sources = SourcesConfig {
                inputs: vec![input.clone(), companion.clone()],
                sources: vec![ConfigSource {
                    inputs: vec![input.name.clone(), companion.name.clone()],
                    targets: vec![playlist_update_target(20, "target")],
                }],
                ..SourcesConfig::default()
            };
            for (index, action) in [
                InputUpdateAction::Provider(InputRefreshPolicy::NORMAL),
                InputUpdateAction::Provider(InputRefreshPolicy::REFRESH),
                InputUpdateAction::Provider(InputRefreshPolicy::FORCE),
                InputUpdateAction::Rescan,
            ]
            .into_iter()
            .enumerate()
            {
                let request = PlaylistUpdateRequestDto {
                    target_ids: Some(vec![20]),
                    input_action: Some(InputUpdateRequest { input_id: 2, action }),
                    ..PlaylistUpdateRequestDto::default()
                };
                let result = super::super::resolve_manual_playlist_update_targets(&sources, &request);
                let expected = enabled && if input_type == InputType::Library { index == 3 } else { index < allowed };
                assert_eq!(result.is_ok(), expected, "{input_type} {action:?}");
                if let Ok(targets) = result {
                    assert_eq!(targets.targets, vec![20]);
                    assert_eq!(targets.inputs, vec![2, 3]);
                }
            }
        }
    }
}

#[tokio::test]
async fn manual_update_library_rescan_uses_same_queue_and_requires_library_permission() {
    use shared::model::{InputUpdateAction, InputUpdateRequest, LibraryConfigDto, WebAuthConfigDto, WebUiConfigDto};
    let input = Arc::new(ConfigInput {
        id: 2,
        name: "library".intern(),
        input_type: InputType::Library,
        enabled: true,
        ..ConfigInput::default()
    });
    let app_config = Arc::new(test_app_config(
        input.clone(),
        ConfigSource { inputs: vec![input.name.clone()], targets: vec![playlist_update_target(20, "target")] },
    ));
    let mut config = (**app_config.config.load()).clone();
    config.library =
        Some(crate::model::LibraryConfig::from(&LibraryConfigDto { enabled: true, ..LibraryConfigDto::default() }));
    config.web_ui = Some(crate::model::WebUiConfig::from(&WebUiConfigDto {
        auth: Some(WebAuthConfigDto::default()),
        ..WebUiConfigDto::default()
    }));
    app_config.config.store(Arc::new(config));
    let mut sources = (**app_config.sources.load()).clone();
    sources.inputs.push(Arc::new(ConfigInput {
        id: 3,
        name: "other-input".intern(),
        input_type: InputType::M3u,
        enabled: true,
        ..ConfigInput::default()
    }));
    sources.sources.push(ConfigSource {
        inputs: vec!["other-input".intern()],
        targets: vec![playlist_update_target(21, "unrelated-target")],
    });
    app_config.sources.store(Arc::new(sources));
    let (sender, mut receiver) = mpsc::channel(1);
    let app_state = test_app_state_with_manual_update_sender(app_config, sender);
    let action = InputUpdateRequest { input_id: 2, action: InputUpdateAction::Rescan };
    let request = PlaylistUpdateRequestDto {
        target_ids: Some(vec![20]),
        input_action: Some(action),
        ..PlaylistUpdateRequestDto::default()
    };
    let mut claims: shared::model::Claims =
        serde_json::from_value(serde_json::json!({"username":"test", "iss":"test", "iat":0, "exp":0})).unwrap();
    let rejected = super::super::playlist_update(
        State(app_state.clone()),
        Some(axum::Extension(crate::api::auth_middleware::VerifiedClaims(claims.clone()))),
        Json(PlaylistUpdateRequestPayload::Current(request.clone())),
    )
    .await
    .into_response();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    assert!(receiver.try_recv().is_err());
    let unauthenticated = super::super::playlist_update(
        State(app_state.clone()),
        None,
        Json(PlaylistUpdateRequestPayload::Current(request.clone())),
    )
    .await
    .into_response();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert!(receiver.try_recv().is_err());
    claims.permissions.set(shared::model::permission::Permission::LibraryWrite);
    // The legacy-name compatibility must never relax the new Rescan wire form.
    let invalid_requests = [
        PlaylistUpdateRequestDto { target_ids: None, ..request.clone() },
        PlaylistUpdateRequestDto { targets: vec!["target".to_string()], target_ids: None, ..request.clone() },
        PlaylistUpdateRequestDto { target_ids: Some(Vec::new()), ..request.clone() },
        PlaylistUpdateRequestDto { target_ids: Some(vec![99]), ..request.clone() },
        PlaylistUpdateRequestDto { target_ids: Some(vec![21]), ..request.clone() },
        PlaylistUpdateRequestDto { targets: vec!["target".to_string()], ..request.clone() },
        PlaylistUpdateRequestDto {
            input_refresh: Some(InputRefreshOverride { input_id: 2, policy: InputRefreshPolicy::NORMAL }),
            ..request.clone()
        },
    ];
    for invalid in invalid_requests {
        let rejected = super::super::playlist_update(
            State(app_state.clone()),
            Some(axum::Extension(crate::api::auth_middleware::VerifiedClaims(claims.clone()))),
            Json(PlaylistUpdateRequestPayload::Current(invalid)),
        )
        .await
        .into_response();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(receiver.try_recv().is_err());
    }
    let accepted = super::super::playlist_update(
        State(app_state),
        Some(axum::Extension(crate::api::auth_middleware::VerifiedClaims(claims))),
        Json(PlaylistUpdateRequestPayload::Current(request)),
    )
    .await
    .into_response();
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    let queued = receiver.recv().await.unwrap();
    assert_eq!(queued.input_action, Some(action));
    assert_eq!(queued.targets.targets, vec![20]);
}

#[tokio::test]
async fn manual_update_run_target_ids_and_refresh_override_keep_the_accepted_queue_identity() {
    let input = Arc::new(ConfigInput {
        id: 17,
        name: "stable-input".intern(),
        input_type: InputType::Xtream,
        enabled: true,
        ..ConfigInput::default()
    });
    let app_config = Arc::new(test_app_config(
        Arc::clone(&input),
        ConfigSource {
            inputs: vec![Arc::clone(&input.name)],
            targets: vec![playlist_update_target(1, "shared-name"), playlist_update_target(4, "shared-name")],
        },
    ));
    let (sender, mut receiver) = mpsc::channel(1);
    let app_state = test_app_state_with_manual_update_sender(app_config, sender);
    let input_refresh = InputRefreshOverride { input_id: input.id, policy: InputRefreshPolicy::FORCE };

    let response = super::super::playlist_update(
        State(app_state),
        None,
        Json(PlaylistUpdateRequestPayload::Current(PlaylistUpdateRequestDto {
            targets: Vec::new(),
            target_ids: Some(vec![4, 1]),
            input_refresh: Some(input_refresh),
            input_action: None,
        })),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("accepted response body");
    let accepted = serde_json::from_slice::<OperationRunAccepted>(&body).expect("accepted response payload");
    let queued = receiver.recv().await.expect("accepted update request");
    assert_eq!(accepted.run_id.as_ref(), Some(&queued.run_id));
    assert!(queued.targets.enabled);
    assert_eq!(queued.targets.inputs, vec![input.id]);
    assert_eq!(queued.targets.targets, vec![4, 1]);
    assert_eq!(queued.targets.target_names, vec!["shared-name", "shared-name"]);
    assert_eq!(
        queued.input_action,
        Some(shared::model::InputUpdateRequest {
            input_id: input_refresh.input_id,
            action: shared::model::InputUpdateAction::Provider(input_refresh.policy)
        })
    );
}

#[tokio::test]
async fn manual_update_target_ids_reject_unknown_empty_and_ambiguous_selections() {
    let input = Arc::new(ConfigInput {
        id: 17,
        name: "stable-input".intern(),
        input_type: InputType::Xtream,
        enabled: true,
        ..ConfigInput::default()
    });
    let app_config = Arc::new(test_app_config(
        Arc::clone(&input),
        ConfigSource {
            inputs: vec![Arc::clone(&input.name)],
            targets: vec![playlist_update_target(1, "known-target")],
        },
    ));
    let (sender, mut receiver) = mpsc::channel(1);
    let app_state = test_app_state_with_manual_update_sender(app_config, sender);
    let input_refresh = Some(InputRefreshOverride { input_id: input.id, policy: InputRefreshPolicy::REFRESH });
    let invalid_requests = [
        PlaylistUpdateRequestDto {
            targets: vec!["known-target".to_string()],
            input_action: Some(shared::model::InputUpdateRequest {
                input_id: input.id,
                action: shared::model::InputUpdateAction::Provider(InputRefreshPolicy::REFRESH),
            }),
            ..PlaylistUpdateRequestDto::default()
        },
        PlaylistUpdateRequestDto { targets: Vec::new(), target_ids: Some(vec![99]), input_refresh, input_action: None },
        PlaylistUpdateRequestDto {
            targets: Vec::new(),
            target_ids: Some(Vec::new()),
            input_refresh,
            input_action: None,
        },
        PlaylistUpdateRequestDto {
            targets: vec!["known-target".to_string()],
            target_ids: Some(vec![1]),
            input_refresh,
            input_action: None,
        },
    ];

    for request in invalid_requests {
        let response = super::super::playlist_update(
            State(Arc::clone(&app_state)),
            None,
            Json(PlaylistUpdateRequestPayload::Current(request)),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(receiver.try_recv().is_err());
    }
}

#[tokio::test]
async fn manual_update_target_ids_preserve_named_legacy_and_empty_bulk_requests() {
    let input = Arc::new(ConfigInput {
        id: 17,
        name: "stable-input".intern(),
        input_type: InputType::Xtream,
        enabled: true,
        ..ConfigInput::default()
    });
    let app_config = Arc::new(test_app_config(
        Arc::clone(&input),
        ConfigSource {
            inputs: vec![Arc::clone(&input.name)],
            targets: vec![playlist_update_target(1, "shared-name"), playlist_update_target(4, "shared-name")],
        },
    ));
    let (sender, mut receiver) = mpsc::channel(1);
    let app_state = test_app_state_with_manual_update_sender(app_config, sender);
    let requests = [
        PlaylistUpdateRequestPayload::Current(PlaylistUpdateRequestDto {
            targets: vec!["shared-name".to_string()],
            target_ids: None,
            input_refresh: None,
            input_action: None,
        }),
        PlaylistUpdateRequestPayload::LegacyTargets(vec!["shared-name".to_string()]),
        PlaylistUpdateRequestPayload::Current(PlaylistUpdateRequestDto {
            targets: Vec::new(),
            target_ids: None,
            input_refresh: None,
            input_action: None,
        }),
    ];

    for (index, request) in requests.into_iter().enumerate() {
        let response =
            super::super::playlist_update(State(Arc::clone(&app_state)), None, Json(request)).await.into_response();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let queued = receiver.recv().await.expect("accepted legacy request");
        assert_eq!(queued.input_action, None);
        if index < 2 {
            assert!(queued.targets.enabled);
            assert_eq!(queued.targets.targets, vec![1, 4]);
            assert_eq!(queued.targets.target_names, vec!["shared-name", "shared-name"]);
        } else {
            assert!(!queued.targets.enabled);
            assert!(queued.targets.targets.is_empty());
            assert!(queued.targets.target_names.is_empty());
        }
    }
}

#[tokio::test]
async fn manual_update_m3u_force_is_queued_by_stable_ids_and_preserves_conflict() {
    use shared::model::{InputUpdateAction, InputUpdateRequest};
    let input = Arc::new(ConfigInput {
        id: 17,
        name: "m3u-force".intern(),
        input_type: InputType::M3u,
        enabled: true,
        ..ConfigInput::default()
    });
    let config = Arc::new(test_app_config(
        input.clone(),
        ConfigSource {
            inputs: vec![input.name.clone()],
            targets: vec![playlist_update_target(1, "selected"), playlist_update_target(2, "other")],
        },
    ));
    let (sender, mut receiver) = mpsc::channel(1);
    let app_state = test_app_state_with_manual_update_sender(config, sender);
    let action =
        InputUpdateRequest { input_id: input.id, action: InputUpdateAction::Provider(InputRefreshPolicy::FORCE) };
    let request = PlaylistUpdateRequestPayload::Current(PlaylistUpdateRequestDto {
        target_ids: Some(vec![1]),
        input_action: Some(action),
        ..PlaylistUpdateRequestDto::default()
    });
    let accepted =
        super::super::playlist_update(State(app_state.clone()), None, Json(request.clone())).await.into_response();
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    let conflict = super::super::playlist_update(State(app_state), None, Json(request)).await.into_response();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let queued = receiver.try_recv().unwrap();
    assert_eq!(queued.input_action, Some(action));
    assert_eq!(queued.targets.targets, [1]);
    assert_eq!(queued.targets.inputs, [17]);
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn manual_update_legacy_input_refresh_preserves_named_and_all_scope_policy_and_queue_conflicts() {
    let input = Arc::new(ConfigInput {
        id: 17,
        name: "provider".intern(),
        input_type: InputType::Xtream,
        enabled: true,
        ..ConfigInput::default()
    });
    let app_config = Arc::new(test_app_config(
        input.clone(),
        ConfigSource {
            inputs: vec![input.name.clone()],
            targets: vec![playlist_update_target(1, "selected"), playlist_update_target(4, "other")],
        },
    ));
    let (sender, mut receiver) = mpsc::channel(1);
    let app_state = test_app_state_with_manual_update_sender(app_config, sender);
    for policy in [InputRefreshPolicy::NORMAL, InputRefreshPolicy::REFRESH, InputRefreshPolicy::FORCE] {
        for names in [vec!["selected".to_string()], Vec::new()] {
            let expected =
                app_state.app_config.sources.load().validate_targets((!names.is_empty()).then_some(&names)).unwrap();
            let request: PlaylistUpdateRequestPayload = serde_json::from_value(json!({
                "targets": names,
                "input_refresh": {"input_id": input.id, "policy": policy}
            }))
            .unwrap();
            let response = super::super::playlist_update(State(app_state.clone()), None, Json(request.clone()))
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let accepted: OperationRunAccepted = serde_json::from_slice(&body).unwrap();
            let conflict =
                super::super::playlist_update(State(app_state.clone()), None, Json(request)).await.into_response();
            assert_eq!(conflict.status(), StatusCode::CONFLICT);
            let queued = receiver.try_recv().unwrap();
            assert_eq!(accepted.run_id.as_ref(), Some(&queued.run_id));
            assert_eq!(queued.targets.enabled, expected.enabled);
            assert_eq!(queued.targets.inputs, expected.inputs);
            assert_eq!(queued.targets.targets, expected.targets);
            assert_eq!(queued.targets.target_names, expected.target_names);
            assert_eq!(
                queued.input_action,
                Some(shared::model::InputUpdateRequest {
                    input_id: input.id,
                    action: shared::model::InputUpdateAction::Provider(policy),
                })
            );
            assert!(receiver.try_recv().is_err());
        }
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn m3u_update_quality_playlist_update_status_reads_newest_timestamp_from_canonically_resolved_input_path() {
    let temp_dir = tempdir().expect("temp dir");
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "timestamped/provider legacy".intern(),
        input_type: InputType::Xtream,
        enabled: true,
        ..ConfigInput::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: Vec::new() };
    let app_config = test_app_config(Arc::clone(&input), source);
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Config::default() }));
    let mut sources = app_config.sources.load().as_ref().clone();
    sources.inputs.push(Arc::new(ConfigInput {
        id: 8,
        name: "never-updated".intern(),
        input_type: InputType::Stalker,
        enabled: true,
        ..ConfigInput::default()
    }));
    sources.inputs.push(Arc::new(ConfigInput {
        id: 9,
        name: "m3u-with-legacy-cluster".intern(),
        input_type: InputType::M3u,
        enabled: true,
        ..ConfigInput::default()
    }));
    app_config.sources.store(Arc::new(sources));

    let never_updated_path = temp_dir.path().join("input_never_updated");
    assert!(!never_updated_path.exists());
    let storage_path = crate::processing::input_cache::resolve_input_storage_path(
        temp_dir.path().to_string_lossy().as_ref(),
        &input.name,
    )
    .await;
    assert_eq!(storage_path.file_name().and_then(|name| name.to_str()), Some("input_timestamped_provider_legacy"));
    let mut persisted = crate::processing::input_cache::InputStatus::default();
    persisted.clusters.insert(
        XtreamCluster::Live.as_ref().to_string(),
        crate::processing::input_cache::ClusterStatus {
            status: crate::processing::input_cache::ClusterState::Ok,
            timestamp: 101,
            last_update: None,
        },
    );
    persisted.clusters.insert(
        XtreamCluster::Video.as_ref().to_string(),
        crate::processing::input_cache::ClusterStatus {
            status: crate::processing::input_cache::ClusterState::Failed,
            timestamp: 303,
            last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                policy: Some(InputRefreshPolicy::REFRESH),
                source: Some(PlaylistUpdateDataSource::Provider),
                quality_guard_threshold: None,
                quality: Some(PersistedPlaylistUpdateQualitySnapshot {
                    threshold: 95,
                    baseline_count: Some(4_812),
                    candidate_count: Some(3_104),
                    achieved_quality: Some(64),
                    decision: PersistedPlaylistUpdateQualityDecision::Rejected,
                }),
                active_count: Some(4_812),
                technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
            }),
        },
    );
    persisted.clusters.insert(
        XtreamCluster::Series.as_ref().to_string(),
        crate::processing::input_cache::ClusterStatus {
            status: crate::processing::input_cache::ClusterState::Ok,
            timestamp: 202,
            last_update: None,
        },
    );
    crate::processing::input_cache::save_input_status(&storage_path, &persisted);

    let m3u_storage_path = crate::processing::input_cache::resolve_input_storage_path(
        temp_dir.path().to_string_lossy().as_ref(),
        "m3u-with-legacy-cluster",
    )
    .await;
    let mut m3u_persisted = crate::processing::input_cache::InputStatus::default();
    m3u_persisted.clusters.insert(
        XtreamCluster::Live.as_ref().to_string(),
        crate::processing::input_cache::ClusterStatus {
            status: crate::processing::input_cache::ClusterState::Failed,
            timestamp: 404,
            last_update: None,
        },
    );
    crate::processing::input_cache::save_input_status(&m3u_storage_path, &m3u_persisted);

    let app_state = test_app_state(Arc::new(app_config));
    let router = super::super::v1_api_playlist_register_protected(Router::new()).with_state(app_state);
    let response = router
        .into_service::<Body>()
        .oneshot(Request::builder().uri("/playlist/update/status").body(Body::empty()).expect("request"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
    let status = serde_json::from_slice::<PlaylistUpdateStatusDto>(&body).expect("status response");
    assert_eq!(status.inputs.len(), 3);
    assert_eq!(status.inputs[0].input_id, 7);
    assert_eq!(status.inputs[0].last_update, Some(303));
    assert_eq!(status.inputs[0].clusters.len(), 3);
    assert_eq!(status.inputs[0].clusters[0].cluster, XtreamCluster::Live);
    assert_eq!(status.inputs[0].clusters[0].status, PersistedPlaylistUpdateClusterState::Ok);
    assert_eq!(status.inputs[0].clusters[0].timestamp, 101);
    assert_eq!(status.inputs[0].clusters[1].cluster, XtreamCluster::Video);
    assert_eq!(status.inputs[0].clusters[1].status, PersistedPlaylistUpdateClusterState::Failed);
    assert_eq!(status.inputs[0].clusters[1].timestamp, 303);
    assert_eq!(
        status.inputs[0].clusters[1].last_update,
        Some(PersistedPlaylistUpdateClusterSnapshot {
            policy: Some(InputRefreshPolicy::REFRESH),
            source: Some(PlaylistUpdateDataSource::Provider),
            quality_guard_threshold: None,
            quality: Some(PersistedPlaylistUpdateQualitySnapshot {
                threshold: 95,
                baseline_count: Some(4_812),
                candidate_count: Some(3_104),
                achieved_quality: Some(64),
                decision: PersistedPlaylistUpdateQualityDecision::Rejected,
            }),
            active_count: Some(4_812),
            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
        })
    );
    assert_eq!(status.inputs[0].clusters[2].cluster, XtreamCluster::Series);
    assert_eq!(status.inputs[0].clusters[2].status, PersistedPlaylistUpdateClusterState::Ok);
    assert_eq!(status.inputs[0].clusters[2].timestamp, 202);
    assert_eq!(status.inputs[1].input_id, 8);
    assert_eq!(status.inputs[1].last_update, None);
    assert!(status.inputs[1].clusters.is_empty());
    assert_eq!(status.inputs[2].input_id, 9);
    assert_eq!(status.inputs[2].last_update, Some(404));
    assert_eq!(status.inputs[2].clusters.len(), 1, "Only the actually persisted M3U cluster is exposed");
    assert_eq!(status.inputs[2].clusters[0].cluster, XtreamCluster::Live);
    assert_eq!(status.inputs[2].clusters[0].status, PersistedPlaylistUpdateClusterState::Failed);
    assert!(never_updated_path.is_dir());
}

#[tokio::test]
async fn playlist_update_status_reader_runs_off_the_async_worker() {
    use crate::processing::input_cache::{
        load_input_status, save_input_status, ClusterState, ClusterStatus, InputStatus,
    };

    let temp = tempdir().unwrap();
    let expected = InputStatus {
        clusters: HashMap::from([(
            "live".to_owned(),
            ClusterStatus { status: ClusterState::Ok, timestamp: 17, last_update: None },
        )]),
        ..InputStatus::default()
    };
    save_input_status(temp.path(), &expected);
    let before = std::fs::read(temp.path().join("status.json")).unwrap();
    let worker = std::thread::current().id();
    let path = temp.path().to_path_buf();
    let actual = super::super::read_playlist_update_input_status(move || {
        assert_ne!(std::thread::current().id(), worker);
        load_input_status(&path)
    })
    .await
    .unwrap();

    assert_eq!(actual.clusters, expected.clusters);
    assert_eq!(std::fs::read(temp.path().join("status.json")).unwrap(), before);
}

#[tokio::test]
async fn playlist_update_status_reader_join_failure_returns_server_error_not_empty_status() {
    let result = super::super::read_playlist_update_input_status(|| panic!("simulated status reader failure")).await;
    assert_eq!(result.unwrap_err(), StatusCode::INTERNAL_SERVER_ERROR);
}
