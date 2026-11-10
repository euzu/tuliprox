use super::{playlist_update_target, test_app_config, test_app_state, test_app_state_with_manual_update_sender};
use crate::model::{Config, ConfigInput, ConfigSource, SourcesConfig};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
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
        PlaylistUpdateRequestPayload, PlaylistUpdateStatusDto, PlaylistUpdateViewSnapshotDto, XtreamCluster,
    },
    utils::Internable,
};
use std::{collections::HashMap, sync::Arc};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tower::ServiceExt;

#[tokio::test]
async fn manual_update_scope_rejects_reused_target_ids_before_queueing() {
    let input = Arc::new(ConfigInput {
        id: 2,
        name: "batch".intern(),
        t_batch_url: Some("batch://local.csv".to_owned()),
        enabled: true,
        ..Default::default()
    });
    let config = Arc::new(test_app_config(
        input,
        ConfigSource { inputs: vec!["batch".intern()], targets: vec![playlist_update_target(1, "selected")] },
    ));
    let mut sources = (*config.sources.load_full()).clone();
    sources.inputs.push(Arc::new(ConfigInput {
        id: 3,
        name: "companion".intern(),
        enabled: true,
        ..Default::default()
    }));
    sources.sources[0].inputs.push("companion".intern());
    config.sources.store(Arc::new(sources));
    let (sender, mut queue) = mpsc::channel(1);
    let app_state = test_app_state_with_manual_update_sender(config.clone(), sender);
    let routes = Router::new()
        .route("/status", axum::routing::get(super::super::playlist_update_status_response))
        .route("/update", axum::routing::post(super::super::playlist_update))
        .with_state(app_state);
    let response =
        routes.clone().oneshot(Request::get("/status?include_scope=true").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let snapshot: PlaylistUpdateViewSnapshotDto = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot.scope.inputs[0].input_type, InputType::M3uBatch);
    assert_eq!(snapshot.scope.sources[0].targets[0].name, "selected");

    let request = |body| {
        Request::post("/update")
            .header("content-type", "application/json")
            .header("if-match", &snapshot.revision)
            .body(Body::from(body))
            .unwrap()
    };
    let bodies = [
        r#"{"targets":[],"target_ids":[1]}"#,
        r#"{"targets":[],"target_ids":[1],"input_refresh":{"input_id":3,"policy":{"cache":"respect","quality":"enforce"}}}"#,
        r#"{"target_ids":[1],"input_action":{"input_id":2,"action":{"kind":"provider","policy":{"cache":"bypass","quality":"enforce"}}}}"#,
    ];
    for body in bodies {
        assert_eq!(routes.clone().oneshot(request(body)).await.unwrap().status(), StatusCode::ACCEPTED);
        let accepted = queue.try_recv().unwrap();
        assert_eq!(accepted.targets.target_names, ["selected"]);
        assert_eq!(accepted.expected_scope.as_ref(), Some(&snapshot.scope));
    }

    let mut sources = (*config.sources.load_full()).clone();
    sources.sources[0].targets = vec![playlist_update_target(1, "other"), playlist_update_target(2, "selected")];
    config.sources.store(Arc::new(sources));
    for body in bodies {
        assert_eq!(routes.clone().oneshot(request(body)).await.unwrap().status(), StatusCode::PRECONDITION_FAILED);
        assert!(queue.try_recv().is_err());
    }
}

#[test]
fn manual_update_capabilities_reject_unsupported_actions_and_keep_all_required_inputs() {
    use shared::model::{InputUpdateAction, InputUpdateRequest};
    for (input_type, allowed) in [
        (InputType::Xtream, 3),
        (InputType::Stalker, 3),
        (InputType::M3u, 3),
        (InputType::Plex, 2),
        (InputType::Library, 1),
        (InputType::Jellyfin, 2),
        (InputType::Emby, 2),
        (InputType::M3uBatch, 3),
        (InputType::XtreamBatch, 3),
        (InputType::StalkerBatch, 3),
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
        HeaderMap::new(),
        Json(PlaylistUpdateRequestPayload::Current(request.clone())),
    )
    .await
    .into_response();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    assert!(receiver.try_recv().is_err());
    let unauthenticated = super::super::playlist_update(
        State(app_state.clone()),
        None,
        HeaderMap::new(),
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
            HeaderMap::new(),
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
        HeaderMap::new(),
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
        HeaderMap::new(),
        Json(PlaylistUpdateRequestPayload::Current(PlaylistUpdateRequestDto {
            targets: Vec::new(),
            target_ids: Some(vec![4, 1]),
            input_refresh: Some(input_refresh),
            input_action: None,
            replace_target_contents: false,
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
        PlaylistUpdateRequestDto {
            targets: Vec::new(),
            target_ids: Some(vec![99]),
            input_refresh,
            ..Default::default()
        },
        PlaylistUpdateRequestDto {
            targets: Vec::new(),
            target_ids: Some(Vec::new()),
            input_refresh,
            input_action: None,
            replace_target_contents: false,
        },
        PlaylistUpdateRequestDto {
            targets: vec!["known-target".to_string()],
            target_ids: Some(vec![1]),
            input_refresh,
            input_action: None,
            replace_target_contents: false,
        },
    ];

    for request in invalid_requests {
        let response = super::super::playlist_update(
            State(Arc::clone(&app_state)),
            None,
            HeaderMap::new(),
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
            replace_target_contents: false,
        }),
        PlaylistUpdateRequestPayload::LegacyTargets(vec!["shared-name".to_string()]),
        PlaylistUpdateRequestPayload::Current(PlaylistUpdateRequestDto {
            targets: Vec::new(),
            target_ids: None,
            input_refresh: None,
            input_action: None,
            replace_target_contents: false,
        }),
    ];

    for (index, request) in requests.into_iter().enumerate() {
        let response =
            super::super::playlist_update(State(Arc::clone(&app_state)), None, HeaderMap::new(), Json(request))
                .await
                .into_response();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let queued = receiver.recv().await.expect("accepted legacy request");
        assert_eq!(queued.input_action, None);
        if index < 2 {
            assert!(queued.targets.enabled);
            assert_eq!(queued.targets.targets, vec![1, 4]);
            assert_eq!(queued.targets.target_names, vec!["shared-name", "shared-name"]);
        } else {
            assert!(!queued.targets.enabled);
            assert_eq!(queued.targets.targets.len(), 0);
            assert_eq!(queued.targets.target_names.len(), 0);
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
        super::super::playlist_update(State(app_state.clone()), None, HeaderMap::new(), Json(request.clone()))
            .await
            .into_response();
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    let conflict =
        super::super::playlist_update(State(app_state), None, HeaderMap::new(), Json(request)).await.into_response();
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
            let response =
                super::super::playlist_update(State(app_state.clone()), None, HeaderMap::new(), Json(request.clone()))
                    .await
                    .into_response();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let accepted: OperationRunAccepted = serde_json::from_slice(&body).unwrap();
            let conflict =
                super::super::playlist_update(State(app_state.clone()), None, HeaderMap::new(), Json(request))
                    .await
                    .into_response();
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
    assert_eq!(status.inputs[1].clusters.len(), 0);
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

#[tokio::test]
async fn replace_target_contents_requires_write_scope_and_eligible_explicit_targets() {
    use crate::{
        auth::create_jwt_web_user,
        model::{TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput},
    };
    use shared::model::{Permission, UserId, WebAuthConfigDto, WebUiConfigDto};

    let mut eligible = playlist_update_target(1, "eligible");
    Arc::make_mut(&mut eligible).output.push(TargetOutput::Xtream(XtreamTargetOutput {
        flags: XtreamTargetFlagsSet::new(),
        trakt: None,
        filter: None,
    }));
    let mut other = eligible.clone();
    Arc::make_mut(&mut other).id = 2;
    Arc::make_mut(&mut other).name = "other".into();
    let mut mixed = eligible.clone();
    Arc::make_mut(&mut mixed).id = 3;
    Arc::make_mut(&mut mixed).name = "mixed".into();
    Arc::make_mut(&mut mixed).output.push(TargetOutput::M3u(crate::model::M3uTargetOutput {
        filename: None,
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
    }));
    let input = Arc::new(ConfigInput { id: 4, name: "provider".intern(), enabled: true, ..Default::default() });
    let config = Arc::new(test_app_config(
        input.clone(),
        ConfigSource { inputs: vec![input.name.clone()], targets: vec![eligible, other, mixed] },
    ));
    let auth =
        WebAuthConfigDto { issuer: "local-fixture".into(), secret: "fixture signing key".into(), ..Default::default() };
    let web = crate::model::WebUiConfig::from(&WebUiConfigDto { auth: Some(auth), ..Default::default() });
    let token = |name, permission: Permission| {
        create_jwt_web_user(web.auth.as_ref().unwrap(), name, permission.into(), 0, UserId::web(name)).unwrap()
    };
    let writer = token("writer", Permission::PlaylistWrite);
    let reader = token("reader", Permission::PlaylistRead);
    config.config.store(Arc::new(Config { web_ui: Some(web), ..Config::default() }));
    let revision = config.sources.load().playlist_update_scope().revision();
    let (sender, mut queue) = mpsc::channel(1);
    let app = test_app_state_with_manual_update_sender(config, sender);
    let router = super::super::v1_api_playlist_register_with_permissions(Router::new(), &app).with_state(app);
    let post = |token: &str, scope: Option<&str>, body: serde_json::Value| {
        let mut request = Request::post("/playlist/update")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"));
        if let Some(scope) = scope {
            request = request.header("if-match", scope);
        }
        router.clone().oneshot(request.body(Body::from(body.to_string())).unwrap())
    };
    let valid = json!({"target_ids":[2,1],"replace_target_contents":true});
    for (token, scope, body, status) in [
        (&reader, Some(revision.as_str()), valid.clone(), StatusCode::FORBIDDEN),
        (&writer, None, valid.clone(), StatusCode::PRECONDITION_REQUIRED),
        (&writer, Some("\"stale\""), valid.clone(), StatusCode::PRECONDITION_FAILED),
        (&writer, Some(&revision), json!({"replace_target_contents":true}), StatusCode::BAD_REQUEST),
        (&writer, Some(&revision), json!({"target_ids":[],"replace_target_contents":true}), StatusCode::BAD_REQUEST),
        (&writer, Some(&revision), json!({"target_ids":[1,3],"replace_target_contents":true}), StatusCode::BAD_REQUEST),
        (
            &writer,
            Some(&revision),
            json!({"target_ids":[1],"replace_target_contents":true,
            "input_refresh":{"input_id":4,"policy":{"cache":"bypass","quality":"enforce"}}}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(post(token, scope, body).await.unwrap().status(), status);
        assert!(queue.try_recv().is_err());
    }
    assert_eq!(post(&writer, Some(&revision), valid).await.unwrap().status(), StatusCode::ACCEPTED);
    let accepted = queue.try_recv().unwrap();
    assert_eq!(accepted.targets.targets, [2, 1]);
    assert!(accepted.replace_target_contents);
    assert_eq!(accepted.expected_scope.unwrap().revision(), revision);

    let ordinary = json!({"target_ids":[3],"replace_target_contents":false});
    assert_eq!(post(&writer, None, ordinary).await.unwrap().status(), StatusCode::ACCEPTED);
    assert!(!queue.try_recv().unwrap().replace_target_contents);
}
