use crate::api::{
    api_utils::stream_json_or_bin_response_stream, auth_middleware::permission_layer,
    endpoints::extract_accept_header::ExtractAcceptHeader, model::AppState,
};
use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, put},
    Json, Router,
};
use shared::{
    error::TuliproxError,
    model::{
        permission::Permission, TargetBouquetDto, TargetBouquetStatusDto, TargetBouquetStreamEventDto, XtreamCluster,
    },
};
use std::sync::Arc;
use tuliprox_core::model::{ConfigInput, ConfigInputFlags, ConfigTarget, SourcesConfig};

pub fn target_bouquet_api_register_with_permissions(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    let read_routes = Router::new()
        .route("/target-bouquets/status", get(get_target_bouquet_status))
        .route("/target-bouquets/groups", get(get_target_bouquet_groups))
        .layer(permission_layer!(app_state, Permission::PlaylistRead));

    let write_routes = Router::new()
        .route("/target-bouquets/selection", put(save_target_bouquet_handler))
        .route("/target-bouquets/selection", delete(delete_target_bouquet_handler))
        .layer(permission_layer!(app_state, Permission::PlaylistWrite));

    Router::new().merge(read_routes).merge(write_routes)
}

pub fn target_bouquet_api_register_unprotected() -> Router<Arc<AppState>> {
    Router::new()
        .route("/target-bouquets/status", get(get_target_bouquet_status))
        .route("/target-bouquets/groups", get(get_target_bouquet_groups))
        .route("/target-bouquets/selection", put(save_target_bouquet_handler))
        .route("/target-bouquets/selection", delete(delete_target_bouquet_handler))
}

#[derive(serde::Deserialize)]
struct TargetBouquetQuery {
    target: String,
}

async fn send_stream_event(
    tx: &tokio::sync::mpsc::Sender<TargetBouquetStreamEventDto>,
    event: TargetBouquetStreamEventDto,
) -> bool {
    tx.send(event).await.is_ok()
}

fn resolve_target_with_inputs(
    sources: &SourcesConfig,
    target_name: &str,
) -> Option<(Arc<ConfigTarget>, Vec<Arc<ConfigInput>>)> {
    sources.sources.iter().find_map(|source| {
        let target = source.targets.iter().find(|target| target.name == target_name)?;
        let inputs = source
            .inputs
            .iter()
            .filter_map(|input_name| sources.inputs.iter().find(|input| input.name == *input_name).cloned())
            .collect();
        Some((Arc::clone(target), inputs))
    })
}

async fn target_summary(state: &AppState, target_name: &str) -> Result<Option<TargetBouquetStatusDto>, TuliproxError> {
    let sources = state.app_config.sources.load();
    let target =
        sources.sources.iter().flat_map(|source| &source.targets).find(|target| target.name == target_name).cloned();
    drop(sources);
    let Some(target) = target else { return Ok(None) };
    let bouquet = tuliprox_repository::load_target_bouquet(&state.app_config, &target.name).await?;
    let active_bouquet = bouquet.as_ref().filter(|value| !value.is_unrestricted());
    Ok(Some(TargetBouquetStatusDto {
        name: target.name.clone(),
        mode: active_bouquet.map(|value| value.bouquet.mode),
        group_count: active_bouquet.map_or(0, |value| {
            [&value.bouquet.groups.live, &value.bouquet.groups.vod, &value.bouquet.groups.series]
                .into_iter()
                .flatten()
                .map(Vec::len)
                .sum()
        }),
    }))
}

async fn stream_input_clusters(
    tx: &tokio::sync::mpsc::Sender<TargetBouquetStreamEventDto>,
    state: &AppState,
    input: &ConfigInput,
    storage_dir: &str,
) -> bool {
    if !send_stream_event(tx, TargetBouquetStreamEventDto::InputStarted { input: input.name.to_string() }).await {
        return false;
    }

    let input_storage = tuliprox_repository::build_input_storage_path(&input.name, storage_dir);
    let clusters = [
        (XtreamCluster::Live, !input.has_flag(ConfigInputFlags::SkipLive)),
        (XtreamCluster::Video, !input.has_flag(ConfigInputFlags::SkipVod)),
        (XtreamCluster::Series, !input.has_flag(ConfigInputFlags::SkipSeries)),
    ];

    let mut group_count = 0;
    let mut missing_clusters = Vec::new();
    for (cluster, enabled) in clusters {
        if !enabled {
            continue;
        }
        match tuliprox_repository::load_raw_group_catalog(
            &input_storage,
            &input.name,
            cluster,
            &state.app_config.file_locks,
        )
        .await
        {
            Ok(Some(cat)) => {
                if cat.groups.is_empty() {
                    let event = TargetBouquetStreamEventDto::InputChunk {
                        input: input.name.to_string(),
                        cluster,
                        groups: Vec::new(),
                        is_last_for_cluster: true,
                    };
                    if !send_stream_event(tx, event).await {
                        return false;
                    }
                } else {
                    let chunk_size = 500;
                    let total_groups = cat.groups.len();
                    let mut groups_iter = cat.groups.into_iter();
                    let mut sent = 0;
                    while sent < total_groups {
                        let take_count = chunk_size.min(total_groups - sent);
                        let chunk: Vec<String> = groups_iter.by_ref().take(take_count).collect();
                        sent += chunk.len();
                        group_count += chunk.len();
                        let event = TargetBouquetStreamEventDto::InputChunk {
                            input: input.name.to_string(),
                            cluster,
                            groups: chunk,
                            is_last_for_cluster: sent == total_groups,
                        };
                        if !send_stream_event(tx, event).await {
                            return false;
                        }
                        tokio::task::yield_now().await;
                    }
                }
            }
            Ok(None) => {
                missing_clusters.push(cluster);
            }
            Err(err) => {
                let event = TargetBouquetStreamEventDto::InputWarning {
                    input: input.name.to_string(),
                    message: format!(
                        "Failed to load raw group catalog for input '{}' ({}): {err}",
                        input.name,
                        cluster.as_ref(),
                    ),
                };
                if !send_stream_event(tx, event).await {
                    return false;
                }
            }
        }
    }

    if !missing_clusters.is_empty() {
        let cluster_labels = missing_clusters.iter().map(std::convert::AsRef::as_ref).collect::<Vec<_>>().join(", ");
        let event = TargetBouquetStreamEventDto::InputWarning {
            input: input.name.to_string(),
            message: format!(
                "Raw group catalog for input '{}' ({cluster_labels}) is not available. Please trigger a playlist update.",
                input.name
            ),
        };
        if !send_stream_event(tx, event).await {
            return false;
        }
    }

    send_stream_event(
        tx,
        TargetBouquetStreamEventDto::InputFinished { input: input.name.to_string(), groups: group_count },
    )
    .await
}

async fn get_target_bouquet_status(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<TargetBouquetQuery>,
) -> Response {
    match target_summary(&app_state, &query.target).await {
        Ok(Some(target)) => Json(target).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Target '{}' not found", query.target) })),
        )
            .into_response(),
        Err(err) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": err.to_string() }))).into_response()
        }
    }
}

async fn get_target_bouquet_groups(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<TargetBouquetQuery>,
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
) -> Response {
    let sources = app_state.app_config.sources.load();
    let Some((target, inputs)) = resolve_target_with_inputs(&sources, &query.target) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Target '{}' not found", query.target) })),
        )
            .into_response();
    };
    drop(sources);

    let (selection, selection_warning) =
        match tuliprox_repository::load_target_bouquet(&app_state.app_config, &target.name).await {
            Ok(bouquet) => (bouquet.map(|value| value.bouquet), None),
            Err(err) => (
                None,
                Some(format!(
                    "Stored target bouquet for '{}' could not be loaded: {err}. Reset or save it to replace the invalid configuration.",
                    target.name
                )),
            ),
        };
    let target_name = target.name.clone();

    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let state = Arc::clone(&app_state);
    tokio::spawn(async move {
        if !send_stream_event(&tx, TargetBouquetStreamEventDto::Selection { bouquet: selection }).await {
            return;
        }
        if let Some(message) = selection_warning {
            let warning = TargetBouquetStreamEventDto::InputWarning { input: target_name, message };
            if !send_stream_event(&tx, warning).await {
                return;
            }
        }
        let storage_dir = state.app_config.config.load().storage_dir.clone();
        for input in inputs {
            if !stream_input_clusters(&tx, &state, &input, &storage_dir).await {
                return;
            }
        }
        let _ = send_stream_event(&tx, TargetBouquetStreamEventDto::Complete).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut response = stream_json_or_bin_response_stream(accept.as_deref(), stream);
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn save_target_bouquet_handler(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<TargetBouquetQuery>,
    Json(bouquet): Json<TargetBouquetDto>,
) -> Response {
    let sources = app_state.app_config.sources.load();
    let Some(target) =
        sources.sources.iter().flat_map(|source| &source.targets).find(|target| target.name == query.target)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Target '{}' not found", query.target) })),
        )
            .into_response();
    };

    match tuliprox_repository::save_target_bouquet(&app_state.app_config, &target.name, bouquet).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response(),
        Err(err) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": err.to_string() }))).into_response()
        }
    }
}

async fn delete_target_bouquet_handler(
    State(app_state): State<Arc<AppState>>,
    Query(query): Query<TargetBouquetQuery>,
) -> Response {
    let sources = app_state.app_config.sources.load();
    let Some(target) =
        sources.sources.iter().flat_map(|source| &source.targets).find(|target| target.name == query.target)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Target '{}' not found", query.target) })),
        )
            .into_response();
    };

    match tuliprox_repository::delete_target_bouquet(&app_state.app_config, &target.name).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response(),
        Err(err) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": err.to_string() }))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::create_test_app_state;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use shared::model::{
        ConfigInputDto, ConfigSourceDto, ConfigTargetDto, PlaylistClusterBouquetDto, SourcesConfigDto,
        TargetBouquetMode,
    };
    use std::sync::Arc;
    use tempfile::TempDir;
    use tower::ServiceExt;
    use tuliprox_core::model::{Config, SourcesConfig};

    fn test_config_with_sources(temp_dir: &TempDir) -> (Config, SourcesConfig) {
        let mut config = Config::default();
        config.storage_dir = temp_dir.path().to_string_lossy().to_string();

        let sources_dto = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto {
                    name: "provider1".to_string().into(),
                    url: "http://localhost:8080/playlist.m3u".to_string(),
                    ..Default::default()
                },
                ConfigInputDto {
                    name: "unconnected".to_string().into(),
                    url: "http://localhost:8080/unconnected.m3u".to_string(),
                    ..Default::default()
                },
            ],
            sources: vec![ConfigSourceDto {
                inputs: vec!["provider1".to_string().into()],
                targets: vec![ConfigTargetDto { name: "living_room".to_string(), ..Default::default() }],
            }],
            ..Default::default()
        };
        let sources_config = SourcesConfig::try_from(&sources_dto).unwrap();
        (config, sources_config)
    }

    fn setup_test_app_state(temp_dir: &TempDir) -> Arc<AppState> {
        let (config, sources) = test_config_with_sources(temp_dir);
        let app_state = create_test_app_state(config);
        app_state.app_config.sources.store(Arc::new(sources));
        app_state.app_config.paths.store(Arc::new(shared::model::ConfigPaths {
            home_path: String::new(),
            config_path: temp_dir.path().to_string_lossy().to_string(),
            storage_path: temp_dir.path().to_string_lossy().to_string(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        }));
        app_state
    }

    #[tokio::test]
    async fn target_bouquets_crud_lifecycle() {
        let temp_dir = TempDir::new().unwrap();
        let app_state = setup_test_app_state(&temp_dir);

        let router = target_bouquet_api_register_unprotected().with_state(Arc::clone(&app_state));

        // An unconfigured target starts without a restricted selection.
        let req = Request::builder()
            .method("GET")
            .uri("/target-bouquets/groups?target=living_room")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let events: Vec<TargetBouquetStreamEventDto> = serde_json::from_slice(&body).unwrap();
        assert!(matches!(events.first(), Some(TargetBouquetStreamEventDto::Selection { bouquet: None })));

        // Persist a restricted selection.
        let bouquet_dto = TargetBouquetDto::new(
            TargetBouquetMode::Blacklist,
            PlaylistClusterBouquetDto { live: Some(vec!["News".to_string()]), vod: None, series: None },
        );
        let req = Request::builder()
            .method("PUT")
            .uri("/target-bouquets/selection?target=living_room")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&bouquet_dto).unwrap()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The lightweight target summary exposes the mode and selected group count for the target editor.
        let req = Request::builder()
            .method("GET")
            .uri("/target-bouquets/status?target=living_room")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let target: TargetBouquetStatusDto = serde_json::from_slice(&body).unwrap();
        assert_eq!(target.mode, Some(TargetBouquetMode::Blacklist));
        assert_eq!(target.group_count, 1);

        // Subsequent reads expose the persisted selection.
        let req = Request::builder()
            .method("GET")
            .uri("/target-bouquets/groups?target=living_room")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let events: Vec<TargetBouquetStreamEventDto> = serde_json::from_slice(&body).unwrap();
        assert!(matches!(
            events.first(),
            Some(TargetBouquetStreamEventDto::Selection { bouquet: Some(bouquet) })
                if bouquet.mode == TargetBouquetMode::Blacklist
                    && bouquet.groups.live == Some(vec!["News".to_string()])
        ));

        // Reset the selection.
        let req = Request::builder()
            .method("DELETE")
            .uri("/target-bouquets/selection?target=living_room")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Reset restores the unrestricted state.
        let req = Request::builder()
            .method("GET")
            .uri("/target-bouquets/groups?target=living_room")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let events: Vec<TargetBouquetStreamEventDto> = serde_json::from_slice(&body).unwrap();
        assert!(matches!(events.first(), Some(TargetBouquetStreamEventDto::Selection { bouquet: None })));
    }

    #[tokio::test]
    async fn invalid_saved_bouquet_keeps_the_editor_available_for_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let app_state = setup_test_app_state(&temp_dir);
        let bouquet_path = tuliprox_repository::target_bouquet_path(temp_dir.path(), "living_room");
        tokio::fs::create_dir_all(bouquet_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(bouquet_path, "version: 1\ntarget: living_room\ngroups:\n  live:\n    - News\n")
            .await
            .unwrap();

        let router = target_bouquet_api_register_unprotected().with_state(app_state);
        let request = Request::builder()
            .method("GET")
            .uri("/target-bouquets/groups?target=living_room")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let events: Vec<TargetBouquetStreamEventDto> = serde_json::from_slice(&body).unwrap();
        assert!(matches!(events.first(), Some(TargetBouquetStreamEventDto::Selection { bouquet: None })));
        assert!(events.iter().any(|event| matches!(
            event,
            TargetBouquetStreamEventDto::InputWarning { input, message }
                if input == "living_room"
                    && message.contains("could not be loaded")
                    && message.contains("Reset or save")
        )));
    }

    #[tokio::test]
    async fn target_bouquet_group_stream_contains_only_target_inputs() {
        let temp_dir = TempDir::new().unwrap();
        let app_state = setup_test_app_state(&temp_dir);

        // Publish catalog for provider1 Live
        let input_storage =
            tuliprox_repository::build_input_storage_path("provider1", temp_dir.path().to_str().unwrap());
        tokio::fs::create_dir_all(&input_storage).await.unwrap();
        tuliprox_repository::publish_raw_group_catalog(
            &input_storage,
            "provider1",
            XtreamCluster::Live,
            vec!["Sports".to_string(), "News".to_string()],
            &app_state.app_config.file_locks,
        )
        .await
        .unwrap();
        let unconnected_storage =
            tuliprox_repository::build_input_storage_path("unconnected", temp_dir.path().to_str().unwrap());
        tokio::fs::create_dir_all(&unconnected_storage).await.unwrap();
        tuliprox_repository::publish_raw_group_catalog(
            &unconnected_storage,
            "unconnected",
            XtreamCluster::Live,
            vec!["Must not leak".to_string()],
            &app_state.app_config.file_locks,
        )
        .await
        .unwrap();

        let router = target_bouquet_api_register_unprotected().with_state(Arc::clone(&app_state));

        let req = Request::builder()
            .method("GET")
            .uri("/target-bouquets/status?target=living_room")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let target: TargetBouquetStatusDto = serde_json::from_slice(&body).unwrap();
        assert_eq!(target.name, "living_room");
        assert_eq!(target.mode, None);
        assert_eq!(target.group_count, 0);

        let req = Request::builder()
            .method("GET")
            .uri("/target-bouquets/groups?target=living_room")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(), shared::utils::CONTENT_TYPE_JSON);
        assert_eq!(resp.headers().get(axum::http::header::CACHE_CONTROL).unwrap(), "no-store");

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let events: Vec<TargetBouquetStreamEventDto> = serde_json::from_slice(&body).unwrap();
        assert!(!events.iter().any(|event| match event {
            TargetBouquetStreamEventDto::InputStarted { input }
            | TargetBouquetStreamEventDto::InputFinished { input, .. }
            | TargetBouquetStreamEventDto::InputWarning { input, .. }
            | TargetBouquetStreamEventDto::InputChunk { input, .. } => input == "unconnected",
            _ => false,
        }));
        assert!(matches!(events.first(), Some(TargetBouquetStreamEventDto::Selection { bouquet: None })));
        let chunk = events
            .iter()
            .find(|event| matches!(event, TargetBouquetStreamEventDto::InputChunk { .. }))
            .expect("live input chunk");
        match chunk {
            TargetBouquetStreamEventDto::InputChunk { input, cluster, groups, is_last_for_cluster } => {
                assert_eq!(input, "provider1");
                assert_eq!(*cluster, XtreamCluster::Live);
                assert_eq!(groups, &vec!["News".to_string(), "Sports".to_string()]);
                assert!(is_last_for_cluster);
            }
            _ => panic!("Expected InputChunk event"),
        }
        let warning = events
            .iter()
            .find(|event| matches!(event, TargetBouquetStreamEventDto::InputWarning { .. }))
            .expect("grouped vod/series warning");
        match warning {
            TargetBouquetStreamEventDto::InputWarning { input, message } => {
                assert_eq!(input, "provider1");
                assert_eq!(
                    message,
                    "Raw group catalog for input 'provider1' (video, series) is not available. Please trigger a playlist update."
                );
            }
            _ => panic!("Expected InputWarning event"),
        }
        assert!(matches!(events.last(), Some(TargetBouquetStreamEventDto::Complete)));
    }
}
