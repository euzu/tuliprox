use crate::api::{auth_middleware::ApiUserContext, model::AppState};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json, Router,
};
use shared::model::{WatchProgressKind, WatchProgressUpdateDto};
use std::sync::Arc;

#[derive(Debug, serde::Deserialize)]
pub struct ProgressListQuery {
    pub kind: Option<WatchProgressKind>,
    pub watched: Option<bool>,
    pub limit: Option<usize>,
}

/// Resolves the watch progress database path after validating feature enablement.
#[allow(clippy::result_large_err)]
pub(crate) fn resolve_progress_path(
    app_state: &AppState,
    api_user: &ApiUserContext,
) -> Result<std::path::PathBuf, Response> {
    if !api_user.user.player_features.watch_progress {
        return Err((StatusCode::FORBIDDEN, "feature_disabled").into_response());
    }
    let paths = app_state.app_config.paths.load();
    tuliprox_repository::watch_progress_repository::watch_progress_path(
        std::path::Path::new(&paths.config_path),
        &api_user.user.username,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Returns a list of watch progress entries for the caller, filtered and newest first.
pub async fn get_me_progress_list(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Query(query): Query<ProgressListQuery>,
) -> Response {
    let progress_path = match resolve_progress_path(&app_state, &api_user) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    match tuliprox_repository::watch_progress_repository::get_progress_list(
        &progress_path,
        &app_state.app_config.file_locks,
        query.kind,
        query.watched,
        limit,
    )
    .await
    {
        Ok(dtos) => Json(dtos).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Returns a single watch progress entry for a given media kind and item ID.
pub async fn get_me_progress_one(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Path((kind, item_id)): Path<(WatchProgressKind, u32)>,
) -> Response {
    let progress_path = match resolve_progress_path(&app_state, &api_user) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if item_id == 0 {
        return (StatusCode::BAD_REQUEST, "invalid_item_id").into_response();
    }
    match tuliprox_repository::watch_progress_repository::get_progress_one(
        &progress_path,
        &app_state.app_config.file_locks,
        kind,
        item_id,
    )
    .await
    {
        Ok(Some(dto)) => Json(dto).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Creates or updates watch progress for a given media kind and item ID.
pub async fn put_me_progress(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Path((kind, item_id)): Path<(WatchProgressKind, u32)>,
    Json(body): Json<WatchProgressUpdateDto>,
) -> Response {
    let progress_path = match resolve_progress_path(&app_state, &api_user) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if item_id == 0 {
        return (StatusCode::BAD_REQUEST, "invalid_item_id").into_response();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let app_config = Arc::clone(&app_state.app_config);
    let username = api_user.user.username.clone();
    match tuliprox_repository::watch_progress_repository::update_progress(
        &progress_path,
        &app_state.app_config.file_locks,
        kind,
        item_id,
        body,
        now,
        move || app_config.get_target_for_username(&username).is_some(),
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(tuliprox_repository::watch_progress_repository::WatchProgressError::UserNotFound) => {
            StatusCode::UNAUTHORIZED.into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Deletes a single watch progress entry.
pub async fn delete_me_progress_one(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Path((kind, item_id)): Path<(WatchProgressKind, u32)>,
) -> Response {
    let progress_path = match resolve_progress_path(&app_state, &api_user) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if item_id == 0 {
        return (StatusCode::BAD_REQUEST, "invalid_item_id").into_response();
    }
    match tuliprox_repository::watch_progress_repository::delete_progress_one(
        &progress_path,
        &app_state.app_config.file_locks,
        kind,
        item_id,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Deletes all watch progress entries for the caller.
pub async fn delete_me_progress_all(State(app_state): State<Arc<AppState>>, api_user: ApiUserContext) -> Response {
    let progress_path = match resolve_progress_path(&app_state, &api_user) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    match tuliprox_repository::watch_progress_repository::delete_all_progress(
        &progress_path,
        &app_state.app_config.file_locks,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Builds the `/me/progress` sub-router with request body limit.
pub fn me_progress_api_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/progress", axum::routing::get(get_me_progress_list).delete(delete_me_progress_all))
        .route(
            "/progress/{kind}/{item_id}",
            axum::routing::get(get_me_progress_one).put(put_me_progress).delete(delete_me_progress_one),
        )
        .layer(DefaultBodyLimit::max(4096))
}

#[cfg(test)]
#[allow(clippy::similar_names, clippy::too_many_lines)]
mod tests {
    use super::*;
    use crate::{
        api::{endpoints::v1_api::v1_api_register, model::create_test_app_state},
        auth::create_jwt_api_user,
        model::{ApiProxyConfig, Config, ConfigSource, ConfigTarget, SourcesConfig},
    };
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use shared::model::{
        ApiProxyConfigDto, PlayerFeaturesDto, ProxyUserCredentialsDto, TargetUserDto, WatchProgressDto,
        WebAuthConfigDto, WebUiConfigDto,
    };
    use tempfile::TempDir;
    use tower::ServiceExt;

    struct TestEnv {
        _dir: TempDir,
        app_state: Arc<AppState>,
        alice_token: String, // watch_progress: true
        carol_token: String, // watch_progress: false
    }

    fn setup_test_env() -> TestEnv {
        let dir = TempDir::new().unwrap();
        let web_auth_dto = WebAuthConfigDto {
            enabled: true,
            issuer: "test".to_string(),
            secret: "test-secret".to_string(),
            ..Default::default()
        };
        let web_ui_dto = WebUiConfigDto {
            enabled: true,
            auth: Some(web_auth_dto.clone()),
            user_ui_enabled: true,
            ..Default::default()
        };
        let config = Config {
            storage_dir: dir.path().to_string_lossy().to_string(),
            web_ui: Some((&web_ui_dto).into()),
            ..Default::default()
        };
        let app_state = create_test_app_state(config);
        {
            let mut paths = (**app_state.app_config.paths.load()).clone();
            paths.config_path = dir.path().to_string_lossy().into_owned();
            app_state.app_config.paths.store(Arc::new(paths));
        }

        let target = Arc::new(ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: "main".into(),
            options: None,
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        let sources = SourcesConfig {
            inputs: Vec::new(),
            sources: vec![ConfigSource { inputs: Vec::new(), targets: vec![Arc::clone(&target)] }],
            ..Default::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let api_proxy_dto = ApiProxyConfigDto {
            user: vec![TargetUserDto {
                target: "main".to_string(),
                credentials: vec![
                    ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 5,
                        player_features: Some(PlayerFeaturesDto { watch_progress: Some(true), ..Default::default() }),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "carol".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 2,
                        player_features: Some(PlayerFeaturesDto { watch_progress: Some(false), ..Default::default() }),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        let mut api_proxy = ApiProxyConfig::from(&api_proxy_dto);
        for tu in &mut api_proxy.user {
            for cred in &mut tu.credentials {
                Arc::make_mut(cred).resolve_plan(&std::collections::HashMap::new());
            }
        }
        app_state.app_config.set_api_proxy(api_proxy).unwrap();

        let web_auth = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();
        let alice_token = create_jwt_api_user(&web_auth, "alice", shared::model::UserId::api("alice")).unwrap();
        let carol_token = create_jwt_api_user(&web_auth, "carol", shared::model::UserId::api("carol")).unwrap();

        TestEnv { _dir: dir, app_state, alice_token, carol_token }
    }

    #[tokio::test]
    async fn endpoint_gate_403_when_watch_progress_disabled() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress")
            .header("Authorization", format!("Bearer {}", env.carol_token))
            .body(Body::empty())
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"feature_disabled");
    }

    #[tokio::test]
    async fn endpoint_put_then_get_exact_body() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        let payload = WatchProgressUpdateDto {
            position_secs: 150,
            duration_secs: Some(300),
            watched: Some(false),
            series_id: Some(10),
        };
        let put_req = Request::builder()
            .method("PUT")
            .uri("/api/v1/me/progress/movie/42")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap();

        let put_res = app.clone().oneshot(put_req).await.unwrap();
        assert_eq!(put_res.status(), StatusCode::NO_CONTENT);

        let get_req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress/movie/42")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();

        let get_res = app.oneshot(get_req).await.unwrap();
        assert_eq!(get_res.status(), StatusCode::OK);
        let body_bytes = to_bytes(get_res.into_body(), usize::MAX).await.unwrap();
        let dto: WatchProgressDto = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(dto.kind, WatchProgressKind::Movie);
        assert_eq!(dto.item_id, 42);
        assert_eq!(dto.position_secs, 150);
        assert_eq!(dto.duration_secs, Some(300));
        assert!(!dto.watched);
        assert_eq!(dto.series_id, Some(10));
        assert!(dto.updated_at > 0);
    }

    #[tokio::test]
    async fn endpoint_list_filters_and_order() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        let paths = env.app_state.app_config.paths.load();
        let progress_path = tuliprox_repository::watch_progress_repository::watch_progress_path(
            std::path::Path::new(&paths.config_path),
            "alice",
        )
        .unwrap();

        // Pre-populate with distinct timestamps: id 1 at 1000, id 2 at 3000, id 3 at 2000
        tuliprox_repository::watch_progress_repository::update_progress(
            &progress_path,
            &env.app_state.app_config.file_locks,
            WatchProgressKind::Movie,
            1,
            WatchProgressUpdateDto {
                position_secs: 100,
                duration_secs: Some(200),
                watched: Some(false),
                series_id: None,
            },
            1000,
            || true,
        )
        .await
        .unwrap();

        tuliprox_repository::watch_progress_repository::update_progress(
            &progress_path,
            &env.app_state.app_config.file_locks,
            WatchProgressKind::Episode,
            2,
            WatchProgressUpdateDto {
                position_secs: 280,
                duration_secs: Some(300),
                watched: Some(true),
                series_id: None,
            },
            3000,
            || true,
        )
        .await
        .unwrap();

        tuliprox_repository::watch_progress_repository::update_progress(
            &progress_path,
            &env.app_state.app_config.file_locks,
            WatchProgressKind::Recording,
            3,
            WatchProgressUpdateDto {
                position_secs: 50,
                duration_secs: Some(100),
                watched: Some(false),
                series_id: None,
            },
            2000,
            || true,
        )
        .await
        .unwrap();

        // List all: newest first
        let get_all = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_all = app.clone().oneshot(get_all).await.unwrap();
        assert_eq!(res_all.status(), StatusCode::OK);
        let list: Vec<WatchProgressDto> =
            serde_json::from_slice(&to_bytes(res_all.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(list.len(), 3);
        // Verify order: newest first (exact ID order: 2 with 3000, 3 with 2000, 1 with 1000)
        assert_eq!(list[0].item_id, 2);
        assert_eq!(list[0].updated_at, 3000);
        assert_eq!(list[1].item_id, 3);
        assert_eq!(list[1].updated_at, 2000);
        assert_eq!(list[2].item_id, 1);
        assert_eq!(list[2].updated_at, 1000);

        // Filter by kind=episode
        let get_episode = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress?kind=episode")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_ep = app.clone().oneshot(get_episode).await.unwrap();
        assert_eq!(res_ep.status(), StatusCode::OK);
        let list_ep: Vec<WatchProgressDto> =
            serde_json::from_slice(&to_bytes(res_ep.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(list_ep.len(), 1);
        assert_eq!(list_ep[0].kind, WatchProgressKind::Episode);
        assert_eq!(list_ep[0].item_id, 2);

        // Filter by watched=true
        let get_watched = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress?watched=true")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_watched = app.clone().oneshot(get_watched).await.unwrap();
        assert_eq!(res_watched.status(), StatusCode::OK);
        let list_watched: Vec<WatchProgressDto> =
            serde_json::from_slice(&to_bytes(res_watched.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(list_watched.len(), 1);
        assert_eq!(list_watched[0].item_id, 2);
    }

    #[tokio::test]
    async fn endpoint_limit() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        for id in 1..=5 {
            let payload =
                WatchProgressUpdateDto { position_secs: 10, duration_secs: Some(100), watched: None, series_id: None };
            let put_req = Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/me/progress/movie/{id}"))
                .header("Authorization", format!("Bearer {}", env.alice_token))
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap();
            let put_res = app.clone().oneshot(put_req).await.unwrap();
            assert_eq!(put_res.status(), StatusCode::NO_CONTENT);
        }

        let get_limit = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress?limit=2")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(get_limit).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let list: Vec<WatchProgressDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn endpoint_delete_one_and_all() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        let payload =
            WatchProgressUpdateDto { position_secs: 10, duration_secs: Some(100), watched: None, series_id: None };
        let put_req = Request::builder()
            .method("PUT")
            .uri("/api/v1/me/progress/movie/1")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap();
        let put_res = app.clone().oneshot(put_req).await.unwrap();
        assert_eq!(put_res.status(), StatusCode::NO_CONTENT);

        // DELETE one
        let del_req = Request::builder()
            .method("DELETE")
            .uri("/api/v1/me/progress/movie/1")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let del_res = app.clone().oneshot(del_req).await.unwrap();
        assert_eq!(del_res.status(), StatusCode::NO_CONTENT);

        // GET one returns 404
        let get_req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress/movie/1")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let get_res = app.clone().oneshot(get_req).await.unwrap();
        assert_eq!(get_res.status(), StatusCode::NOT_FOUND);

        // Put another and DELETE all
        let put_req2 = Request::builder()
            .method("PUT")
            .uri("/api/v1/me/progress/movie/2")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap();
        let _ = app.clone().oneshot(put_req2).await.unwrap();

        let del_all_req = Request::builder()
            .method("DELETE")
            .uri("/api/v1/me/progress")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let del_all_res = app.clone().oneshot(del_all_req).await.unwrap();
        assert_eq!(del_all_res.status(), StatusCode::NO_CONTENT);

        let get_all_req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let get_all_res = app.oneshot(get_all_req).await.unwrap();
        assert_eq!(get_all_res.status(), StatusCode::OK);
        let list: Vec<WatchProgressDto> =
            serde_json::from_slice(&to_bytes(get_all_res.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(list, Vec::new());
    }

    #[tokio::test]
    async fn endpoint_reject_item_id_0_with_400() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        let get_req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress/movie/0")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let get_res = app.clone().oneshot(get_req).await.unwrap();
        assert_eq!(get_res.status(), StatusCode::BAD_REQUEST);

        let put_req = Request::builder()
            .method("PUT")
            .uri("/api/v1/me/progress/movie/0")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"position_secs":10}"#))
            .unwrap();
        let put_res = app.clone().oneshot(put_req).await.unwrap();
        assert_eq!(put_res.status(), StatusCode::BAD_REQUEST);

        let del_req = Request::builder()
            .method("DELETE")
            .uri("/api/v1/me/progress/movie/0")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let del_res = app.oneshot(del_req).await.unwrap();
        assert_eq!(del_res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn endpoint_get_unknown_item_returns_404() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/progress/movie/99999")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn endpoint_rejects_put_body_larger_than_4kib() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        let large_padding = "a".repeat(5000);
        let payload = format!(r#"{{"position_secs":10,"padding":"{large_padding}"}}"#);
        let req = Request::builder()
            .method("PUT")
            .uri("/api/v1/me/progress/movie/1")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(payload))
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn put_progress_for_deleted_user_returns_unauthorized_and_creates_no_file() {
        let env = setup_test_env();
        let app = v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state));

        // Delete alice from config
        if let Some(old_proxy) = env.app_state.app_config.api_proxy.load().clone() {
            let mut new_proxy = (*old_proxy).clone();
            for tu in &mut new_proxy.user {
                tu.credentials.retain(|u| u.username != "alice");
            }
            env.app_state.app_config.api_proxy.store(Some(Arc::new(new_proxy)));
        }

        let paths = env.app_state.app_config.paths.load();
        let progress_path = tuliprox_repository::watch_progress_repository::watch_progress_path(
            std::path::Path::new(&paths.config_path),
            "alice",
        )
        .unwrap();

        let payload =
            WatchProgressUpdateDto { position_secs: 10, duration_secs: Some(100), watched: None, series_id: None };
        let put_req = Request::builder()
            .method("PUT")
            .uri("/api/v1/me/progress/movie/42")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap();
        let put_res = app.oneshot(put_req).await.unwrap();
        assert_eq!(put_res.status(), StatusCode::UNAUTHORIZED);
        assert!(!progress_path.exists(), "watch progress file must not be created for deleted user");
    }

    #[tokio::test]
    async fn test_resolve_progress_path_feature_disabled_returns_403_body() {
        use axum::extract::FromRequestParts;
        let env = setup_test_env();

        let req = Request::builder().header("Authorization", format!("Bearer {}", env.carol_token)).body(()).unwrap();
        let (mut parts, ()) = req.into_parts();
        let user_ctx = ApiUserContext::from_request_parts(&mut parts, &env.app_state).await.unwrap();

        let result = resolve_progress_path(&env.app_state, &user_ctx);
        assert!(result.is_err());
        let response = result.unwrap_err();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body_bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(body_bytes.as_ref(), b"feature_disabled");
    }
}
